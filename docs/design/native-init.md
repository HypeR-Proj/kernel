<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Native init contract

The production kernel boot path starts one Native userspace process from the
firmware-provided initial ramdisk. Linux guest autostart is userspace policy.
In the default deployment, stage-1 init starts core storage and recovery
services, provisionally switches the VFS root, then transfers supervision to the
formal component manager. System startup provisions the VM manager, which creates isolated VM runtimes
through Native capabilities. These stages and paths beyond initial startup are
not kernel requirements. Kernel self-test images contain no Linux loader or
default VM. See the [boot layout](https://github.com/HypeR-Proj/HypeR/blob/main/docs/architecture/boot-layout.md) for the default
deployment and recovery behavior.

The AArch64 reference guest selects GICv2 or GICv3 from the creation lease's
platform metadata. The `aarch64_gic_version` field is 2 or 3 on supported Arm
hosts and zero on other architectures. vm-runtime uses this value to generate
the guest interrupt-controller node; host device-tree addresses are never
copied into guest firmware. Kernel, ABI and SDK consumers must be rebuilt
together for the extended 32-byte platform-info record. Arm guest topology is
fixed by the VM JSON configuration (1..8 vCPUs); RISC-V currently supports one. Init does not
handle guest PSCI: vm-runtime waits on its VM handle for power requests, and
vm-manager owns creation of a fresh instance after guest reset. See
[guest power requests and SMP](vm-bundle.md#guest-power-requests-and-smp).

## Rust runtime

Init uses HypeR's Rust `std`, like the other in-tree applications. It uses
Native SDK bindings for bootstrap handles, capability delegation and service
construction; these operations are not provided by the Rust standard library.
Using `std` does not grant additional authority.

Before `main`, the Native runtime initializes the heap, creates the final main
stack, attaches the initial thread and adopts the capabilities delivered through
the userspace loader's startup vector. This does not require init's child
services to be running. Standard I/O uses delegated stream handles when present
and otherwise falls back to a
bootstrap Console capability, allowing init to report startup errors before
the console services exist.

Init and the other `bootstrap/stage1` programs link statically, independently
of the ordinary application's dynamic delivery graph. Rust `std` support and
dynamic linking are separate choices; using `std` does not require a shared
Rust standard-library image or the formal environment's loader.

## Initial ramdisk

Firmware must describe one initial ramdisk through the architecture boot
protocol. The complete range must lie in discovered RAM, remain disjoint from
the kernel and DTB, and fit in the permanent linear map. The kernel reserves
that physical range for the lifetime of the mounted filesystem.

The archive is an uncompressed SVR4 `newc` or `crc` CPIO stream. Mounting
validates the complete archive and builds a fallible, sorted metadata index;
file names and contents remain borrowed from the immutable ramdisk. Paths are
relative, nonempty UTF-8 names. Empty components, `.` components, `..`, leading
or trailing slashes, duplicate canonical names, and a non-directory root entry
are rejected. A conventional `.` directory entry is accepted but is not
indexed.

The archive is the initial `/`. Its `/init` and
`/lib/userspace-loader-hyper-<arch>` paths must resolve to regular files with an
executable mode bit. Ordinary filesystem symlinks are permitted; `/boot` and
`/system` have no special meaning to the kernel. The in-tree stage-1 loader
accepts static `/init` only; this is its userspace policy, not a kernel
requirement on init. Another userspace model may supply its own loader at the
same path, provided it satisfies the bootstrap ELF and channel contract.
The formal dynamic loader and its libraries may be absent until userspace
mounts a system volume.

## Executable image

Only initial startup selects a loader by pathname. Descendant process builders
receive an executable File with `READ|EXECUTE|PROTECT|DUPLICATE|TRANSFER`
and a loader File with `READ|EXECUTE`. The child receives only
`READ|EXECUTE|PROTECT` on the target; builder creation preserves file identity,
not a frozen content version. The kernel validates and maps the loader's restricted ELF,
then enters it on an empty guarded stack. Its first argument register carries
a ByteChannel handle. The queued 96-byte `loader_startup` header describes the
executable/VMAR capabilities and mapping bounds, plus the child's ExecutionContext,
ObjectFactory and SystemInfo capabilities, followed by tagged capabilities
and opaque SDK data. No runtime or library directory is opened implicitly by
the kernel. Application ELF, argv/auxv and application relocation are userspace
responsibilities. Kernel-image relocation during boot is unchanged.

The [bootstrap loader](https://github.com/HypeR-Proj/bootstrap/blob/main/userspace-loader/README.md) accepts
static core executables. The [formal loader](https://github.com/HypeR-Proj/dynamic-loader/blob/main/README.md)
loads both static and dynamic programs. They share bounded ELF64 mapping and
stack encoding in [lib/dynamic-loader/elf](https://github.com/HypeR-Proj/dynamic-loader/blob/main/elf/README.md), accepting
branded AArch64/RISC-V `ET_EXEC`/`ET_DYN` images and rejecting overlapping load
segments, W+X, nonempty ELF TLS and executable stacks. A dynamic `PT_INTERP`
must match `/lib64/ld-hyper-<arch>.so`; the formal loader validates that identity
instead of opening a second interpreter. Dependencies resolve only through
the delegated architecture library Directory.

The formal loader self-relocates at entry, then owns symbol resolution,
dependency loading, RELA/RELR, RELRO and constructors. Static CRT performs its
own relative relocation. Both paths share the SDK's final-stack handoff.
Relocation can never grant writable access to code; the kernel independently
enforces executable provenance and W^X on mappings.

The [Native 64-bit application ABI](syscall-abi.md#native-64-bit-application-address-space-contract)
uses architecture-specific application and system-managed user regions.
Current application limits are 128 TiB on AArch64 VA48 and 128 GiB on RISC-V
Sv39; these are provisional kernel layout policy. The kernel grants ROOT_VMAR
from one page up to the HAL application address limit.
An `ET_DYN` image is biased so its lowest mapped page begins at `0x40_0000`.
The SDK bounds application mapping spans at 64 MiB.
The kernel supplies a temporary, non-executable 128 KiB bootstrap stack with
its exclusive top at `0x3f_f000`, plus one unmapped guard page at each end.
The kernel entry stack is empty. The userspace loader constructs the
16-byte-aligned `argc`, `argv`, `envp`, and `auxv` handoff itself. The startup
message transfers ownership of the temporary reservation to the SDK.

The SDK initializes the heap and creates the final main stack using the same
allocator as secondary threads. The main image's `PT_GNU_STACK.p_memsz` requests
the final usable size through `HYPER_AUXV_MAIN_STACK_SIZE`; absent/zero selects
256 KiB. The userspace loader validates ELF stack headers. This request does not
change the kernel bootstrap stack size. The runtime rounds the final
size to pages, enlarges tiny requests to hold the entry vector and one page of
handoff headroom, reserves at least 256 MiB of capacity, and keeps a guard page at
each end. Placement prefers the top of the application range; this is SDK
policy, not a fixed-address app ABI. Only the initial usable extent is mapped.

Before any constructors or app entry execute, the runtime copies arguments,
environment strings, auxiliary entries and handle records out of bootstrap
storage, switches SP through an architecture-specific non-returning handoff,
and releases the bootstrap stack reservation from the final stack. Static
programs also release their selected loader; dynamic programs retain the formal
loader for its continuation, constructors and later `dlopen` operations.
It then acknowledges startup to the parent before executing constructors. No old C
frame is resumed. The dynamic loader makes this handoff through the relocated
shared runtime before constructors; static CRT uses the same implementation.
The application startup view omits the consumed bootstrap-stack handle and
reports final-stack geometry. Main and worker stacks share creation, explicit
downward growth and retirement; there is no fixed SDK worker stack arena.
The root capability permits duplication; the heap and stack allocators each
retain a MAP-only process-lifetime duplicate independent of the application's
startup handle. The thread pointer starts at zero; runtime attachment installs
the SDK's per-thread state before application entry. The runtime reserves an
initial heap VMAR of about one eighth of the application address limit, using
`0xe0000000` as a low-end hint after the loader's `[0x20000000, 0xe0000000)`
library range. The returned
base is authoritative. Backing is mapped only as allocations need it; overflow
regions have independent VMARs, so this reservation is not a heap size limit.
The kernel still owns the root address space and
resource accounting; allocator policy lives in `stdlib/hyper`.

The child receives an ordinary executable File. Its userspace loader acquires
`PROTECT` before reading size or ELF bytes, then maps file-backed private
segments. Partial pages are sanitized and BSS is zero-filled. Active mappings
retain protection independently of the parser token and File handles. Clean
pages may be discarded under memory pressure and faulted back from the protected
file; modified private pages remain charged to the process. No whole-file
snapshot is exported to userspace. Owner-process teardown detaches protected
sources after its threads stop; external VMAR owners cannot prolong protection
or allow further file refills in that dead address space. Executable pages are
instruction-coherent before RX publication; relocations target writable private
data and never require a writable code alias. Kernel Process publication occurs
once the bootstrap environment is prepared, before application parsing and
dynamic linking. A loader failure after that point terminates an already
published child; on a failed startup result the SDK requests stop, then waits
for termination. Successful readiness precedes constructors and
`main`, so it does not promise that application initialization will succeed.
Later file-backed instruction or data faults can also fail: exhausted bounded
reclaim/refill retries terminate the Process as an unhandled page fault. Explicit
mapping and copy syscalls instead report allocation, quota or I/O failure statuses.

See the [loader address layout](https://github.com/HypeR-Proj/dynamic-loader/blob/main/elf/README.md#address-layout)
for the current mapping windows and their policy boundaries.

## Bootstrap capabilities

The capability substrate defines the typed objects required by the initial
process: `Process`, `TaskGroup`, `ResourceDomain`, `TaskFactory`,
`ExecutableAuthority`, VMO, and VMAR. Long-lived lifecycle objects and the root
VMAR have one concurrency-safe object-publication identity. Handles remain
process-local generation values, rights may only decrease, and a heterogeneous
initial set can use the existing batch reservation and publication transaction.

The production init transaction reserves and writes only the authorities init
currently consumes: the root `ResourceDomain`, root `TaskGroup`,
`TaskFactory`, root `Directory`, system `TaskInspector`, `ObjectInspector`,
`MemoryInspector` and `CpuInspector` views, the process root VMAR, the
runtime-owned initial stack VMAR, and (when available) Console, VM creation and
device-assignment authorities. Future handle values are encoded while unresolved
and the complete batch is published before the
initial Thread can run. No self-Process handle is installed in its own table.
The root VMAR and executable File are delegated with narrowly typed rights for
runtime linking. File mappings require READ and a matching process-local
protection token, plus EXECUTE for executable segments; writes cannot obtain
executable authority.

The initial root Directory is an explicit namespace-root anchor and includes
`MANAGE_NAMESPACE`. Duplicates retain the anchor; opened directories remain
fixed capabilities. Generic namespace operations permit userspace to mount
ramfs and atomically replace the root. Internal subtree binding is not a public syscall. They do not select
`/boot`, `/system` or any service graph. The default supervision handoff transfers
this management authority to system init; ordinary service and shell directory
grants omit it.

The kernel selects `/init` as the first application's executable and starts it
through the bootstrap userspace loader; it neither interprets the service manifest nor
preloads system services. After handling `earlycon` and `nokaslr`, it supplies
unrecognized arguments in an immutable `BOOT_COMMAND_LINE` VMO to the initial
process: an eight-byte little-endian byte length followed by at most
512 UTF-8 bytes without NUL. Its `READ | INSPECT` rights confer no configuration
or device authority. `Startup::take_boot_command_line` copies the text and closes
the handle, including on read failure. Userspace can prepare a new immutable VMO
with `startup::create_boot_command_line` to pass its unconsumed arguments to a
successor. The kernel does not interpret userspace selectors.

Stage-1 selects `/etc/hyper/boot-profiles/default.json`, or the named profile
selected by `hyper.boot-profile=NAME`, then validates that profile's service and
capability graph before constructing children with `ProcessBuilder`. These
paths and provider choices are userspace policy. Before starting the candidate
system supervisor, stage-1 provisionally switches to the independent root.
The candidate validates the offered inventory and formal provider policy before
accepting the existing Process supervisors. That rendezvous commits supervision
ownership and the candidate commits the root transition. A failed pre-commit
handoff rolls back the provisional root and leaves supervision with stage-1.

A builder owns every staged startup capability immediately after successful
insertion. Starting a
sealed builder publishes the child, atomically replaces the consumed builder
authority with a supervisor Process handle and result channel in the parent, and only then makes
the initial child Thread runnable. The SDK waits for loader completion and
stops/reaps failed children before reporting a committed launch error. Init retains `REQUEST_STOP` on every
supervisor and requests termination of all already-started children if graph
launch or later critical supervision fails.

Init retains physical Console management authority. Separate input and output
workers receive only the physical direction and raw byte-channel direction
they require. The session manager owns the peer data endpoints and receives no
physical Console capability. It establishes a virtual console before creating
its foreground shell with fresh input, output and error channels. The manager
owns that Process supervisor; shell exit or failure triggers a rate-limited new
client, without restarting the manager or physical workers. Init neither launches
nor monitors the shell as a system service.

The manager receives explicit delegatable root Directory, TaskFactory, TaskGroup,
ResourceDomain, child loader, child library and inspection capabilities. It attenuates these
into each shell's startup table. A VM connector is optional; when present, the
manager can duplicate it but each shell gets only `WAIT|WRITE`. Each console
instance keeps its own transport and client lifecycle. The current bootstrap
wires one physical console; additional UART discovery and transport wiring remain
future work. Commands continue to get fresh handle-backed standard I/O, with
blocking multi-object waits rather than polling.

The shell also receives one persistent manager connector with exactly
`WAIT|WRITE`. It cannot duplicate that authority. Each `/bin/vmm` launch
creates private control and capability-channel pairs, transfers only their
manager endpoints through the connector, and gives the client endpoints to the
new process. Consequently an attached guest console cannot monopolize the
manager connection or prevent independent lifecycle requests.

Manifest capability purposes are symbolic service-contract names rather than
raw integers. Init validates the expected object kind, explicit rights and
unique resolved purpose before creating a builder. Built-in console, session,
shell and VM-manager images retain their specialized contracts. Other Native
apps can use standard stdio and process/inspection contracts without an image
allowlist; required rights remain mandatory and optional rights are granted only
when the manifest requests them. A service can therefore receive `WAIT|WRITE`
stdout and `INSPECT` CPU statistics without authority to delegate either.
A read-only std filesystem root needs `READ|INSPECT|DUPLICATE`: the runtime
duplicates directory cursors and opens metadata-capable Files. `WRITE` and
`TRANSFER` remain optional and should be omitted when unnecessary.
Private roles such as `vm.provisioning` do not become generic app capabilities.
The installed Rust SDK carries these typed contracts in `hyper-service`.

The optional `virtual-machines.config` manifest field names the fleet config
file. The supervising init (system init in the default deployment) opens it
through the root Directory and transfers the File capability
to the unique service declaring the VM provisioning contract. The configuration
contains named definitions and autostart policy; a Native-only manifest can omit
it. Guest image paths are inputs to named VM definitions, not guest identities.

Native apps remain the deployment layer for system policy, including future
power-management policy services. The kernel owns privileged mechanisms and
validates their capabilities; policy decisions need not move into the kernel or
Linux I/O VM. This deployment contract does not claim that Native power-control
operations are already implemented.

The userspace service manifest selects `loader: "bootstrap"` for static core
programs and `loader: "dynamic"` for formal programs; omitted means dynamic.
This choice is separate from phase ordering and is not interpreted by the kernel.
Every dynamically linked service receives the loader's library Directory with
exactly `READ|EXECUTE|PROTECT`. Services which construct child processes receive a
second, explicitly declared `process.child-library-directory` capability with
the transport rights required to attenuate it into a child's loader slot. This
keeps ordinary services from inheriting delegation authority merely because
they use the dynamic runtime. A service that launches children also needs an
explicit `process.child-loader` executable capability. Loader and library
capabilities are separate roles; possession of one does not imply the other.

The manifest format reserves restart policies, but the current runtime accepts
only `never` and requires at least one critical service. The physical Console
input and output workers and the session service are critical;
the interactive shell is a replaceable, noncritical client owned by the session
manager. Init observes every
service Process and the fleet's one-shot configuration result in one bounded
`object_wait_many` set without polling. It reports terminal Process information
before releasing each dead supervisor handle. A noncritical service exit is
recorded and removed from the set; a critical exit stops the remaining graph.
The default VM manager is noncritical: unavailable fleet configuration does not
terminate Native storage or recovery. The manager acknowledges admission of the complete
configuration before starting any autostart guest; empty fleets use the same
result. Init then removes the result endpoint and continues supervising the
manager Process. All guest starts, stops, and failures belong to the manager,
independently of definition order. Reliable restart additionally
requires a monotonic backoff facility. Unsupported supervision graphs are
rejected during preflight before any child is started.

## Validation boundary

Host tests validate archive indexing, path rejection, ELF permissions, layout,
and bootstrap entry points. SDK host tests exercise application ELF admission,
startup stack encoding, and supported relocation decoding. Kernel QEMU tests run
standalone mechanism tests and then retire the bootstrap execution. The `test-native` contract separately builds the
Native applications through the assembled SDK, constructs the production
initramfs, and verifies that init loads the manifest, starts the session and
shell Processes, launches `ps` and `handle` through scoped inspection handles,
executes a constructor-bearing shared-object fixture through `dlopen`, runs an
external echo command through the complete physical Console path, and attaches
`/bin/vmm` to the separately buffered guest serial stream. The VM portion must
observe Linux userspace startup, deliver guest-console input, and detach
through the local Ctrl-] menu.

The VM manager uses a std worker Thread for blocking capability rendezvous.
A bounded ownership handoff and ByteChannel notification feed its main wait set;
VM lifecycle state remains on the main Thread. Both Threads share the same
Process and authority table; this is concurrency separation, not isolation. The event loop rotates ready
sources and waits indefinitely when idle, using a finite deadline only for an
outstanding runtime exit grace period. It does not periodically poll for clients.

### Runtime loss during guest CPU power transitions

`make test-power-crash ARCH=aarch64` builds explicitly enabled test runtimes
and runs three restart/reclamation cycles at each of these boundaries:

- all four guest vCPUs installed but still dormant;
- a real Linux `CPU_ON` request pending before userspace accepts it;
- a real Linux `CPU_OFF` request accepted after the requesting CPU detached from
  hardware and entered the Off power state (scheduler parking may still race).

The fixture disables VM autostart to measure an idle memory baseline and choose
each guest's start time. Each abrupt process exit bypasses Rust destructors;
the test verifies the selected boundary marker, failed VM state, exact guest-page
reclamation, bounded runtime-memory retention, and successful new instances.
`test-power-crash` and `HYPER_TEST_POWER_CRASH` are test-only build selections;
ordinary runtime binaries contain neither the hooks nor a control protocol for
triggering them. This is QEMU lifecycle coverage, not physical DMA/cache proof.

`make test-fleet-config` separately checks successful configuration admission for
empty and non-autostart fleets, malformed JSON and image-admission rejection
without partial guest startup, and a guest startup failure that leaves the next
autostart guest and Native shell running. An unavailable host CPU affinity
causes the startup failure after image admission; malformed images remain
configuration errors.
