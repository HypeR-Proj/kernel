<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# VM image and boot ownership

HypeR keeps firmware boot, system userspace, and guest boot as separate trust
and ownership layers. The platform boot chain loads the HypeR kernel and its
system initramfs and describes them through the architecture boot protocol.
QEMU uses direct kernel/initrd loading; Pi 5 uses the official EEPROM firmware
and bundled BL31. This does not imply image authentication or a secure-boot
contract. Firmware does not select, relocate or modify a guest VM.

The system initramfs is an uncompressed SVR4 `newc` or `crc` CPIO archive. It
contains `/init`, system services, SDK runtime libraries, configuration, and
zero or more guest FIT images under `/vm`. Early boot validates and reserves
the complete firmware-provided archive before making it the immutable ramfs
backing for the Native root filesystem.

## Userspace ownership

The initial service graph identifies exactly one VM manager by its validated
provisioning purpose, then starts it inside an init-created fleet resource
domain and task group. Init opens the deployment's VM configuration file and
transfers it through the provisioning channel, with a ByteChannel for initial
boot supervision. The manager validates named definitions and opens their FIT
images through its delegated Directory. Each start gets private runtime control
and console-connector channels; numeric VM IDs are not ambient authority.
Neither the manager nor a business VM runtime receives physical Console authority.

The policy allows eight named business definitions and budgets their instances,
a separate resident I/O VM and control-plane headroom. Each business child domain
allows up to eight vCPUs and 65,536 guest pages (256 MiB). These are admission
ceilings, not reserved RAM or a promise that every guest fits concurrently.
See `lib/vm-policy/src/lib.rs` for the complete limits. A stopped instance can
be recreated from its retained image with fresh resource and lifecycle owners.

For each provisioned VM, the manager creates a child resource domain and task
group, derives a one-shot VM creation lease, and starts an isolated
`/svc/vm-runtime` process. The guest image is duplicated into that runtime.
The runtime receives only its guest image, creation lease, read-and-execute
runtime libraries, its own process resources, and a CapabilityChannel for receiving
authorized console clients.

Each runtime:

1. parses its guest FIT through bounded random-access reads and validates the
   startup configuration supplied by the manager;
2. creates and retains the guest-memory VMO;
3. streams the selected kernel and initramfs payloads into that VMO;
4. constructs guest firmware data, including the Linux DTB;
5. seals a pending VM and atomically publishes the installed VM and boot-vCPU
   handles; and
6. retains supervision handles until acknowledged VM retirement.

The kernel owns stage-2 translation, interrupt virtualization, vCPU execution,
and teardown. It retains an independent reference to the guest-memory backing
through stage-2 invalidation, so process or handle teardown cannot free pages
still reachable by hardware. Attaching the VMO acquires an exclusive hardware
write lease: it fails if a Native writable mapping or direct access is active,
and later direct access, snapshots, or writable mappings stay closed until
acknowledged VM retirement. Read-only Native mappings may coexist.

Stage-2 demand pages are initially read/write and execute-never. A final guest
instruction fault publishes that page's instruction bytes and then promotes
only its stage-2 execute permission using the selected architecture's required
translation invalidation. Faults raised while walking guest page tables remain
data-only. Sealing conservatively publishes and promotes every loader-resident
page because executable-range metadata is not yet part of the VM ABI; sparse
pages retain the demand-promotion path.

A userspace-created VM has no implicit route to the physical host Console.
The runtime creates a VirtualSerial, allocates a 68 KiB VMO, maps it read-only,
and registers those whole pages before assigning the port into the pending VM.
Registration acquires an exclusive write lease: existing writable aliases reject
registration; direct VMO access, snapshots, and new writable aliases remain
excluded until final retirement. The kernel pins the backing but allocates no
private output queue. One header page contains production and loss counters;
the remaining 64 KiB holds atomic byte slots. Runtime reads acquire-observe the
producer and acknowledge absolute consumption through a READ-authorized syscall.
Invalid backwards or future acknowledgements change neither cursor nor signals.

Output publication, acknowledgement, and closure serialize under one port lock.
The first byte of an unread batch asserts READABLE; subsequent bytes do not
notify. Acknowledgement clears it only if all published bytes were consumed.
Thus output arriving before clearing stays readable and output arriving after
clearing reasserts readiness. The existing signal wait protocol closes the
check-to-park race. The AArch64 SBSA UART retains unsent bytes in its TX FIFO
when this ring is full; runtime acknowledgements resume transmission and update
the guest interrupt level. Closing the last runtime handle drains pending TX
into a disconnected sink and updates the interrupt level as well, so a guest
waiting for space need not poll. Other producers drop new bytes and count loss when
full. No producer overwrites unconsumed bytes. See the generated ABI for layout.

The AArch64 reference guest exposes `arm,sbsa-uart` at `0x09000000`, with SPI 1
(INTID 33), fixed 115200 baud / 8N1 and separate 32-byte receive/transmit FIFOs.
The always-enabled SBSA interface includes DR, RSR/ECR, FR, IMSC, RIS, MIS and
ICR. RX/TX interrupts use fixed one-eighth FIFO thresholds; RX timeout expires
after 32 bit periods using the host counter, including while the guest sleeps.
Overrun preserves queued bytes and reports the error separately from interrupt
clearing. The byte transport supplies no parity, framing or break errors.
PL011 baud/configuration, DMA, modem, flow-control and identification registers
are not exposed. Reserved registers read as zero and ignore writes. Byte,
halfword and word accesses at register bases are accepted; byte access to a
16-bit register is an extension of the minimum SBSA interface.

MMIO completes in the kernel. TX completes once the bounded output transport
accepts the byte, with no simulated wire delay or userspace device-model round
trip. FIFO state, byte publication and IRQ state share the UART lock across
vCPUs. A reserved per-VM timer delivers receive timeouts and is retired before
device storage is freed. The device tree supplies `current-speed` rather than
PL011 clock nodes; Linux continues to use `ttyAMA0` and can discover earlycon
through `stdout-path`. RISC-V retains its NS16550 model. Physical host UART
drivers are independent of these guest devices.

`vm-runtime` uses a persistent WaitSet for output, control, vCPU termination,
and actionable client channel states, with an infinite deadline. It owns a
bounded 64 KiB retention queue and nonblocking client forwarding. Guest input
uses bounded partial injection and WRITABLE notification when its queue regains
space. The runtime subscribes to client READABLE only when it can accept another
message, and to client WRITABLE only while output is pending. Peer closure drains
already accepted input. Neither direction requires periodic polling.

The lock order is UART, port, signal state, then scheduler/WaitSet notification.
Input injection and output acknowledgement release the port lock before
updating the UART. Timer control precedes the UART lock; retirement waits for
callbacks only after releasing the UART lock.
Guest-device kicks run after releasing the port lock. Output byte publication
uses Release and readers use Acquire; consumption completes before the syscall
releases slots under the port lock. Atomic byte access also tolerates a hostile
caller prematurely acknowledging slots. Physical AArch64 qualification must
stress publication/acknowledgement across CPUs, idle wakeups, full queues, and
runtime termination during output; QEMU alone does not prove weak ordering.

The shell holds only a `WAIT|WRITE` manager-connector endpoint. Each `/bin/vmm`
invocation obtains private control and capability channels. For `vmm console`,
the manager authorizes one client, creates a ByteChannel pair, sends one end to
the runtime's connector, and transfers the other end to the client. Neither
console bytes nor VirtualSerial handles are relayed through the manager to the
client. Closing the private control channel releases attachment policy; the
runtime observes the data peer closing. Ctrl-] detaches without stopping the VM.

VirtualSerial handles stay in their creating runtime: generic capability
transfer and ProcessBuilder storage are forbidden. The same-process device
assignment consumes only the binding handle. Runtime loss therefore closes all
userspace port handles and synchronously closes serial output admission under
the port lock, after any admitted writer completes. VM stop and acknowledged
retirement quiesce vCPU execution before those owners can release registered
pages. User unmapping, handle close, and runtime address-space teardown therefore
cannot turn a cached kernel pointer into a write to freed/reused memory. Pinned
page charges follow the registration lifetime, independently of user mappings.

The manager validates each runtime's monotonic lifecycle records and records
its terminal outcome in that VM's lifecycle state. Loss of either control peer or
malformed protocol data triggers an idempotent forced-stop transition; it does
not terminate the fleet manager. A client stop first requests cooperative
shutdown. The state machine also defines an explicit grace-deadline escalation
transition. The manager computes a finite absolute deadline from the Native
monotonic clock, passes it directly to the same multi-object wait, and forces
the runtime Process when the deadline expires. It does not approximate time
with polling, scheduler yields, or unrelated inspection authority.

## Serial validation

`make test-native` covers console attachment, output, and detachment.
`make test-runtime-crash` builds the explicit `test-runtime-crash` fixture and
boots a separate archive; the production runtime keeps its default features.
Two guest definitions autostart. Attaching the first guest's console makes its
runtime exit without destructors during console forwarding; the other guest
must still answer live status requests, and the Native shell must remain usable.
A second short boot reverses the definition order and repeats the isolation
check. After stopping the survivor in the first boot, five fresh VM/runtime
cycles with the default 1 GiB QEMU RAM check teardown and continued allocation
against an idle memory baseline.
Kernel self-tests verify invalid registration, hostile cursor values, full-ring
behavior, exclusive assignment, last-handle output closure, and release of
pinned and committed pages only after the final device owner drops. Host tests
exercise cursor wrap and exhaustion.

Physical AArch64 qualification still needs concurrent guest output/runtime
consumption under migration, forced runtime termination, and repeated VM
restart on FEAT_VHE-capable hardware. Check payload ordering and final memory
accounting under load; QEMU does not establish weak-memory or cache behavior.

## Guest FIT contract

Guest images use the standard flattened image tree container with embedded
payload data. The current packer emits this shape:

```text
/
  images/
    kernel@1/
      data
      type = "kernel"
      arch = "arm64"
      os = "linux"
      compression = "none"
      load
      entry
    ramdisk@1/
      data
      type = "ramdisk"
      arch = "arm64"
      os = "linux"
      compression = "gzip"
  configurations/
    default = "conf@1"
    conf@1/
      compatible = "hyper,guest-image-v2"
      kernel = "kernel@1"
      ramdisk = "ramdisk@1"
```

Kernel `load` and `entry` are exactly one 64-bit big-endian value each.
The FIT `configurations` node selects payloads only; it is not VM runtime policy.
The selected record identifies the `hyper,guest-image-v2` storage contract.
VM memory size, vCPU count and boot arguments are required in the external JSON
definition's `configuration`. The reference platform is selected from ITB `arch`.
The manager rejects a foreign architecture before publishing a definition, and
the runtime checks again at every start before allocating guest memory. The parser rejects those legacy
policy properties in a FIT and rejects v1 images. Ramdisk `load` and `entry`
are absent: the runtime computes page-aligned placement at the top of configured
RAM, so changing RAM never requires repacking. Kernel addresses remain payload
metadata checked against the Linux Image header and selected platform.
References and strings are exact, NUL-terminated UTF-8
values. Selected configuration and image records are property-only schema
leaves; child nodes are rejected. Duplicate selected nodes or properties,
embedded NUL bytes, overlapping FDT blocks, invalid ranges, unsupported image
types, and architecture mismatches are rejected before VM construction.

The AArch64 reference runtime supports one to eight vCPUs, power-of-two page-aligned RAM of
at least 64 MiB, an uncompressed Linux kernel, and an optional Linux-supported
compressed initramfs. It validates the raw Linux `Image` magic, entry and
`text_offset` placement, and nonzero `image_size`; placement reserves that
complete occupied extent rather than only the bytes embedded in the FIT. Both
the 2 MiB-aligned placement base and the occupied image extent must lie in
guest RAM. The loader rejects overlap between that extent, the initramfs, and
the generated DTB, and verifies that every selected payload range lies within
the FIT source.
FIT validation also requires an aligned, zero-terminated memory reservation
map which does not overlap the structure or string blocks. Parsing has fixed
budgets for reservation records, structure tokens, and nesting depth; embedded
payload properties are skipped in constant parser work and copied only after
the complete metadata contract succeeds. These are runtime implementation
limits, not a storage-format promise for other architectures or future VMM
implementations.

Manager admission, VM runtime validation and the I/O loader use four temporary
4 KiB windows for sparse FIT metadata reads. Each pass creates its own view;
the runtime revalidates at every start. The view is not an immutable file
snapshot and is discarded before streaming payloads. Bulk payload reads retain
their two-buffer, 512 KiB pipeline rather than being split into metadata pages.

JSON `configuration.vcpus` fixes the machine's immutable processor topology before
construction. AArch64 GICv2 and GICv3 profiles accept 1..8 vCPUs; the RISC-V
reference profile still accepts one. `pending_virtual_machine_set_bootstrap`
configures boot vCPU 0. Before installation publishes the VM, all configured
vCPU endpoints and dormant scheduler Threads are allocated and bound under the
construction transaction. Rollback owns every unpublished Thread. Installation
returns the VM and boot-vCPU handles; `virtual_machine_open_vcpu(machine, id)`
opens another configured endpoint without allocating a CPU or changing topology.
Only vCPU 0 is initially started through `virtual_cpu_start`.

## Guest power requests and SMP

AArch64 guests use PSCI over HVC. `CPU_ON`, `CPU_OFF`, `AFFINITY_INFO`,
`SYSTEM_OFF`, and `SYSTEM_RESET` are implemented alongside the PSCI discovery
calls. CPU and system suspend remain unsupported. Guest power operations never
invoke the host's PSCI firmware interface.

The kernel validates guest arguments and serializes per-vCPU power transitions.
`CPU_ON` reserves an existing powered-off CPU and records its entry/context;
the caller waits for the runtime's decision while other guest CPUs can run.
Successful acceptance supplies fresh architectural state to the target.
`CPU_OFF` parks its caller after acceptance, retaining the same Thread and
endpoint for a later `CPU_ON`. The guest CPU identity remains stable across
off/on cycles; powering off is not VM retirement. `AFFINITY_INFO` reads the
kernel's authoritative power state without a userspace round trip.

Requests are associated with their source vCPU and exposed through one VM-level
wait source. vm-runtime uses a WaitSet alongside console and control events;
there is no per-vCPU userspace waiting thread or polling timer. The Native API
adds three nonblocking VM operations, each requiring WRITE authority:

- `virtual_machine_get_power_request`: snapshot one pending request, or
  `would_block`. Inspection does not consume the request.
- `virtual_machine_complete_power_request`: accept or reject the exact request
  ID. Stale or already completed requests cannot complete a newer operation.
- `virtual_machine_open_vcpu`: return a control handle for a configured CPU.

Waiting on the VM requires WAIT authority. `POWER_REQUEST` indicates pending
work; `VCPU_TERMINATED` reports terminal CPU execution so the runtime can inspect
its retained per-vCPU handles, including failures on secondary CPUs. VM stop or
runtime loss cancels pending operations and retires every configured CPU,
including CPUs that were never powered on.

For `SYSTEM_OFF` and `SYSTEM_RESET`, the calling CPU stops guest execution while
vm-runtime coordinates whole-VM stop and waits for retirement. Poweroff reports
a clean stopped instance. Reset reports `RebootRequested` to vm-manager only
after retirement; the manager waits for successful runtime exit and creates a
fresh instance and lease. Administrative stop wins over a concurrent guest
reset. Reboot disconnects an attached console, which can then attach to the new
instance by its existing VM name.

Both GIC backends provide per-vCPU private interrupts and timers, SGI/IPI routing,
and shared SPI routing. GICv3 firmware includes one redistributor frame per CPU;
GICv2 preserves SGI source identity and its eight-bit target mask. Guest memory
supports concurrent execution while retaining per-host-CPU hardware residency.
An individual vCPU remains exclusively scheduled by its owning Thread. Mapping
changes, executable-code publication, and retirement must account for every CPU
that can retain the guest translation or instructions.

`tools/fit-pack` creates deterministic payload-only development and CI images from an
external kernel and initramfs. Generated guest artifacts remain ignored by Git.

The packer interface is `hyper-fit-pack OUTPUT ARCH KERNEL LOAD ENTRY INITRAMFS`.
It validates payload metadata and Linux kernel headers. Machine dimensions,
boot arguments and initramfs placement are checked by the runtime after combining
the image with external configuration. A single ITB can serve multiple definitions.


## Integrity and licensing

FDT structural validation is not authenticity. Production composition must
authenticate the HypeR kernel, platform DTB, system initramfs, and guest images
according to deployment policy before granting VM creation authority. FIT hash
or signature nodes can be added without moving image selection into U-Boot or
the kernel.

Guest kernels and userspace retain their upstream licenses. Downloaded guest
payloads are test artifacts and are not part of HypeR's Apache-2.0 source.
