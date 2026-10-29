<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Architecture boundaries

HypeR keeps policy above mechanism. A subsystem should expose the smallest
contract its caller needs and keep ownership, lifecycle, address domains, and
hardware effects explicit. This document describes the implemented dependency
model and its ownership contracts. Explicitly identified extensions remain outside the current
implementation.

## Layer model

The kernel workspace has seven crates. An arrow denotes a Cargo dependency:

| Crate | Direct kernel-crate dependencies | Responsibility |
| --- | --- | --- |
| `hyper-core` | None | Shared types, collections, synchronization, hardware contracts and foundational memory mechanisms |
| `hyper-platform` | core | Firmware parsing, resource descriptions and board-specific discovery |
| `hyper-drivers` | core, platform | Physical device implementations and driver binding mechanisms |
| `hyper-vm-models` | core | Guest-visible devices, interrupt models and portable virtualization contracts |
| `hyper-trace` | core | Event definitions, macros and bounded recording |
| `hyper-hal` | core, platform, drivers, vm-models | Selected architecture implementations and machine capabilities |
| `hyper` (binary) | All six libraries | Initialization, resource ownership, scheduling and runtime subsystem policy |

Core additionally consumes the SDK's dependency-free `hyper-abi` crate. The
binary consumes `hyper-filesystem` and optional symbol demangling. These are
external to the seven kernel crates.

```text
kernel binary ──→ HAL ──→ drivers ──→ platform ──→ core
       │           ├───────────────→ platform
       │           ├───→ vm-models ─────────────→ core
       │           └───────────────────────────→ core
       ├───────────→ trace ─────────────────────→ core
       └───────────→ core, platform, drivers, vm-models
```

Each library owns its source under its own `src/` directory. HAL's `arch` and
implementation modules remain private; the binary imports its public interface
as `hal`. Core is imported as `hyper`; other libraries use their explicit crate
names (`hyper_platform`, `hyper_drivers`, `hyper_vm_models`, `hyper_trace`). Core
does not re-export these libraries and cannot depend on them.

The binary's modules live directly under `src/`. `diagnostics/` groups crash,
debug symbols, inspection, logging, PMU profiling and trace control. `fs/` groups
filesystem primitives, boot archives, VFS ownership and file-cache residency.
Bootstrap executable validation belongs to `process/formats/`. Runtime timer
queues belong to `time/queue/`. `mm/allocator/` groups the runtime buddy and heap
implementations with `policy.rs`, which supplies kernel policy and the global
allocator instance. Host tests include the allocator mechanisms independently
of that policy. `vm/serial/` groups virtual serial transport, output registration
and ring arithmetic; `vm/timer/` groups guest-timer binding and recovery.

Within each HAL architecture, `memory/` owns host translation geometry, KASLR
and translation mechanisms, while `irq/` owns physical interrupt control.
`user/` groups Native user execution and address-space machinery on AArch64 and
RISC-V. `vm/` groups guest execution, virtual interrupts and stage-2 translation;
the x86 VMX and SVM backends each own their assembly entry file in that directory.
Assembly moves with its owning subsystem, and `build.rs` names those paths
explicitly. Cross-subsystem CPU context, register definitions and platform
discovery remain architecture-level modules. HAL's public capability interface
and the crate dependency graph are unchanged by this internal grouping.

Core retains address/range types, fallible allocation wrappers, early allocation,
and portable page-table/translation mechanisms. `PhysicalRange` and `RegionList`
live in `core/src/mm/ranges.rs`; platform discovery uses these types. The immutable
`MemoryHandoff` snapshot also belongs to core. Boot allocation produces it, and
the runtime buddy allocator takes its own memory/reservation state from it.
This keeps the early allocator independent of both platform discovery and the
runtime allocator implementation.

Device and virtualization implementations consume core contracts or caller
supplied operations. They do not call into kernel ownership or depend on HAL's
concrete implementation. Trace similarly receives an installed machine adapter:
CPU pinning, clock access, deadlines, authority checks and wire encoding remain
in `src/diagnostics/trace/`, outside the recorder crate.

The dependency gate in `tests/ci/hal-boundary.py` checks all seven packages and
traverses indirect edges, including external helper packages, to reject reverse
or cyclic dependencies. The binary deliberately has no library target.

Cargo dependencies and Rust privacy enforce these access boundaries. They do
not prove lock ordering, memory ordering, or retirement correctness; those
still require explicit protocols, review, and behavioral tests. AArch64 remains
Tier 1: a common interface must not hide behavior required for its correctness
or diagnosis.

## Shared data structures

`hyper::collections` owns reusable algorithms without subsystem authority or
locking policy. Modules define their own entry types, implement the public
traits, and may wrap a container with domain-specific methods.

- `persistent_avl::PersistentAvl` stores generic ordered keys and maintains a
  subtree maximum weight and entry count. `Entry` defines stable keys and
  weights; `()` disables weighted filtering. `NodeAccount` supplies fallible
  per-node accounting.
  Clones share immutable subtrees, and updates return a new root without
  publishing it. Rank selection and allocation-free ordered range iteration
  operate on an immutable root. VMAR identities, free ranges and actual Native
  mapping records wrap this with memory accounting; their authority checks,
  backing retirement and transactional publication stay in `user_space`.
- `indexed_heap` provides allocation-free sift and repair operations over a
  `Storage` adapter. The adapter supplies comparison, storage and swaps that
  update reverse positions. Fixed-capacity deadline queues and vGIC ready queues
  share these operations while retaining their own capacity, handle, deadline
  wraparound and interrupt-priority rules.

- `bounded_vec::BoundedVec` retains an immutable logical capacity and never
  grows after construction. vGIC uses it through a local error adapter.
- `budgeted_vec::BudgetedVec` and `budgeted_string::BudgetedString` retain the caller's storage charge
  through allocation lifetime, including consuming iterators. The shared
  `allocation_account::StorageBudget` trait defines admission and RAII release; filesystem
  facades preserve existing import paths without duplicating implementations.

`byte_ring::ByteRing` provides the byte FIFO shared by console input and output.
The log module re-exports it; record-oriented logging and drain synchronization
remain in the log subsystem.

`fixed_bitmap::FixedBitmap` owns fixed-length word storage, with checked bit
access and an exact payload-size query. VM code retains resource admission and
maps container errors to VM errors; no page state or synchronization lives in
the bitmap.

`linked_list` contains operations over caller-owned endpoints, not an owning
list container. `NextLink` supports insert/remove over exclusively borrowed
successor fields; `TailLink` supports constant-time append/pop/splice for
reference-handle chains. Adapters select the link field and preserve existing
reference retention and lock behavior. Process discovery, IPC receiver queues,
and Process/Object retirement use these helpers. Queue limits, unique
membership, wakeup arbitration, retirement readiness and destruction remain
with their respective subsystems. Scheduler membership transitions are not
replaced by these link operations.

The runtime `OwnedDeadlineQueue` remains a separately owned linked queue. Its
preallocated-node and lock-context requirements are not changed by extracting
these helpers. Containers do not acquire subsystem locks, publish resources,
or infer a caller's allocation policy.

## Placement rules

- Group implementations by subsystem within each crate. Once a category has a
  directory, keep its implementations and binding helpers there rather than
  placing related files beside it. For example, firmware TRNG belongs to
  `drivers/src/random/trng.rs`, and serial-console binding belongs to
  `drivers/src/serial/console.rs`. A standalone module is appropriate for a
  distinct responsibility without a natural parent category, such as the shared
  platform-device contracts in `drivers/src/platform.rs`. This does not merge
  similarly named modules across crate dependency boundaries.
- A module with child files keeps its entry point inside the same directory as
  `mod.rs`; do not split it between `name.rs` and a sibling `name/` directory.
  Leaf modules may remain single files. For example, the NS16550 driver lives
  entirely under `drivers/src/serial/ns16550/`, with `mod.rs`, `access.rs` and
  `platform_binding.rs`.
- The kernel binary owns host policy, resource publication, initialization order, and
  runtime VM/vCPU ownership.
- `hyper-vm-models` owns reusable guest-facing formats and models. It must not own a kernel
  scheduler hook, global VM registry, or selected architecture backend.
- `arch` owns context and exception ABIs, page-table and register encodings,
  instruction execution, CPU-specific behavior, and hardware virtualization
  entry/exit mechanics.
- `hyper::hal` owns reusable capability contracts. `hyper-hal` supplies the thin,
  one-way machine adapter: it may use its private `arch`, but never `kernel`, and
  must not acquire policy ownership or create an alternate entry path.
- `hyper-drivers` owns discoverable physical devices and firmware protocols. Virtual
  devices are VM services or reusable VM models, not physical drivers.
- Early boot code must state which runtime facilities are unavailable. It must
  not silently depend on the permanent heap, scheduler, mappings, or logging.

Architecture-defined data layouts may remain portable and host-testable. The
operation which applies such a layout to hardware belongs to the selected
backend.

## Device-driver placement

An independent, trusted Linux I/O VM is an architectural foundation of HypeR's
physical I/O design. It provides reusable Linux device drivers and exports
services to Native userspace and business guests. It runs as a guest under
HypeR; it is not the host kernel. Storage and guest networking use this path.
Native networking retains a reserved endpoint without an active link.

The selected I/O VM interface families are virtio-scsi for storage, virtio-net
for networking, and vfio-user for general device backends. Virtio-scsi and
virtio-net are implemented with Linux vhost-scsi/LIO and vhost-net/TAP;
vfio-user remains an architectural target. Additional device models require a
concrete use case and an explicit design review; universal device coverage is
not a goal.
The vfio-user target requires a cross-VM transport, memory-authorization and
device-lifecycle design before protocol compatibility can be claimed. It does
not imply automatic forwarding of arbitrary Linux-driven physical devices.

Physical device drivers should live in the I/O VM or HypeR Native userspace
whenever those environments can implement them correctly. Native drivers use
explicit device capabilities and expose application-facing services. Kernel
placement is an exception requiring a documented need, such as early boot,
interrupt control, architectural timers or diagnostic-console availability.
Convenience alone is not sufficient reason to move a driver or service into
the kernel.

The kernel owns device authority, permitted MMIO access, interrupt delivery,
DMA admission and translation, and safe revocation/retirement. Delegating a
driver does not delegate these ownership guarantees, and userspace placement
alone does not establish DMA isolation on hardware without suitable support.
Guests trust the Linux I/O VM for backend data and availability. Host DMA
containment additionally depends on the physical assignment path.

The [SMMUv3 driver](../design/smmuv3.md) provides a host-owned stage-2 DMA mechanism and
a dedicated QEMU PCI DMA acceptance fixture. It reserves SMMU and generic ECAM
control apertures and all their interrupt aliases, defaults to denying
unassigned streams, and retains exposed backing on failed invalidation.
A wired-IRQ worker quarantines faulting streams, while control-plane failures
close the controller with disabled-mode abort policy and retain live backing.
Default QEMU virtio-PCI assignments bind their requester IDs to the I/O VM's
domain before enabling bus mastering. Initial RAM and explicit live backend
grants retain kernel-only memory leases until completed invalidation; full VM
retirement additionally requires physical virtio reset acknowledgement before
stream detachment and domain teardown. Device operation rights and those leases
are integrated with the assignment path. See [I/O isolation](../design/io-isolation.md)
for the ownership contract and qualification boundaries. The dedicated
virtio-mmio compatibility deployment and Pi 5 still trust the I/O VM and have
no equivalent DMA containment.

On Pi 5, HypeR owns the BCM2712 PCIe transport, resource claims and MSI routing.
The complete RP1 PCI function belongs to the I/O VM; Linux owns its interrupt
controller, clocks, GPIO, Ethernet MAC and PHY drivers. RP1 peripherals cannot
be split between owners. This path has no IOMMU DMA isolation, and physical
networking and DMA retirement still require hardware qualification.

This decision does not move the VFS out of the kernel. Namespace semantics,
file handles and VFS policy remain kernel responsibilities; filesystem and
block backends can be supplied by services. Keep driver execution, service
policy and kernel resource lifetime separate. See the
[I/O VM design](https://github.com/HypeR-Proj/HypeR/blob/main/docs/design/virtualization/io-vm.md) for the current transport and shared-memory
contract.

## Selected HAL crate

The selected HAL crate exposes eleven capability modules:

- `hal::context` owns schedulable register images, context switching, and the
  final stack-reset transition; task lifecycle and stack mapping remain kernel
  policy;
- `hal::cpu` owns typed logical/hardware CPU identity conversion, secondary
  entry mechanisms, processor lifecycle events, and firmware power binding;
- `hal::exception` owns exception-vector installation, crash-context capture,
  emergency-stack entry, and crash-stop delivery; stop/resume decisions remain
  behind the kernel entry adapters;
- `hal::irq` owns ordinary reversible local masking, fail-stop source masking,
  host-controller construction, platform interrupt decoding, and targeted
  reschedule notification;
- `hal::memory` owns host stage-1 construction and activation, address
  translation, local execution protection, and synchronous shared stage-1
  invalidation;
- `hal::cache` owns cache geometry and explicit data/instruction publication,
  while `hal::atomic` exposes immutable runtime atomic-backend diagnostics;
- `hal::platform` owns allocation-free essential-device discovery,
  architecture KASLR geometry, optional host port I/O, and runtime
  architecture diagnostics;
- `hal::time` owns the boot-entry counter origin, monotonic counter, one-shot
  comparator, and validated kernel timer description;
- `hal::user` owns user-address limits, prepared translation roots,
  CPU-affine activation tokens, user-context entry, and typed return
  completion; process policy and syscall dispatch remain in the kernel;
- `hal::vm` owns stage-2 translation, vCPU entry, virtual interrupt hardware,
  guest timer integration, and architecture-local exit completion. Reusable
  device models live in `vm`; each installed VM owns its mutable instances in
  `kernel::vm::device`. Guest exits cross registered entry services as owned,
  fixed-width events with exhaustive actions; raw frames and backend
  completion state remain architecture-private.

Guest backing and translation leaf size are separate contracts. AArch64 may
map already-resident, aligned contiguous RAM with 2 MiB Stage-2 blocks while
retaining 4 KiB allocation/accounting and MMIO isolation. Kernel VM policy
proves whole-range ownership and uniform execute authority; the HAL owns leaf
selection, descriptor inspection and block demotion. Demotion prepares the
replacement table before breaking the old descriptor, invalidates the exact
VMID's translations, and publishes the table without moving physical backing.
Range revocation prepares all fallible structural changes before withdrawing
software admission; old backing survives until hardware invalidation and DMA
quiescence complete. Existing page tables are not automatically coalesced.

AArch64 requires FEAT_VHE, checked before establishing the host translation
regime. The configured lower range belongs exclusively to Native Process roots;
the canonical upper range contains MMIO, linear RAM, the randomized kernel image,
and guarded kernel stacks. The permanent-memory handoff remains opaque above
`hal::memory` and carries the split-root state required by secondary CPUs.
Hardware without FEAT_VHE is unsupported. Primary and secondary CPUs validate
the feature before accessing VHE-specific registers. An unsupported CPU stops
in the pre-MMU `aarch64_unsupported_vhe` loop, observable through a debugger;
this early path does not assume a platform UART is available.

The checked [AArch64 address layout](../../hal/src/arch/aarch64/memory/address_layout.rs)
defines permanent host windows for each supported `CONFIG_ARM64_VA_BITS`
value from 42 through 48. Representative bases are:

| Window | 48-bit VA base | 42-bit VA base |
| --- | --- | --- |
| Canonical upper range | `0xffff000000000000` | `0xfffffc0000000000` |
| MMIO aliases | `0xffff100000000000` | `0xfffffc8000000000` |
| RAM direct map | `0xffff400000000000` | `0xfffffd0000000000` |
| Kernel image / KASLR window | `0xffffff0000000000` | `0xffffff0000000000` |
| Bootstrap stack reservation | `0xffffff8000000000` | `0xffffff8000000000` |
| Runtime stack arena | `0xffffff8000200000` | `0xffffff8000200000` |

These are reserved virtual windows, not fully populated mappings. MMIO and
RAM aliases add the physical address to the corresponding base and must remain
inside that window. Only discovered, admitted ranges are mapped. The kernel
image occupies a checked, 2 MiB-aligned position within its 512 GiB KASLR
window. The following 2 MiB reservation holds the bootstrap stack, with an
unmapped guard page below its 256 KiB payload. Runtime stack slots likewise
retain a low guard page and admit at most 64 payload pages; checked slot
arithmetic must reject overflow into another window or beyond the address
space. The unused upper-range prefix remains unmapped.

Linux guest image validation, layout, DTB construction and boot-register plans
belong to the userspace VM image/runtime stack. The kernel validates Native VM
authority and platform contracts; the HAL realizes register plans and supplies
host virtualization mechanisms. Kernel tests use minimal dormant VM fixtures
without embedding a Linux loader.

Only the HAL crate can access its private architecture backend. Adding a
kernel-policy dependency to that crate would create a Cargo dependency cycle;
upward runtime transitions instead use explicitly registered services. Boundary
checks should protect the package dependency direction and private exports,
not require particular local variable names or call spellings.

Allocation-free FDT discovery currently keeps its bounded collector scratch on
the active boot stack. Linker bootstrap stacks and post-translation CPU0 boot
stacks therefore retain 256 KiB on every architecture, guarded by source and
linked-image CI checks. Moving that scratch into explicit caller-owned boot
storage is required before this temporary stack margin may be reduced.

## Exception and VM-exit entry adapters

Exceptions and VM exits enter from a low-level implementation but need a
kernel policy decision. This necessary upward transition belongs in one small
adapter per entry class:

1. Decode the raw architecture frame into a typed event.
2. Invoke one explicitly registered kernel service boundary.
3. Encode the returned action into machine state.

Fatal, physical-interrupt, and VM-exit services are immutable one-shot
registrations. Runtime vector installation and vCPU construction require the
corresponding readiness capability; entry before publication halts in the
architecture backend. Decode and completion remain allocation-free, keep local
interrupts masked. Native syscall entry retains its private frame on the
Thread stack while its owned invocation runs in interruptible Thread context;
no private-frame reference enters policy. Other architecture mechanism code must not call kernel
logging, boot failure, scheduling, IRQ dispatch, or VM policy directly.

Synchronous physical-IRQ dispatch also carries an opaque, CPU-affine
`hal::irq::InterruptFrame` view. Its borrow ends before IRQ-tail scheduling;
handlers cannot access or mutate the architecture-private frame. Passing the
view does not copy registers. A handler selected by normal IRQ-domain routing
may explicitly request an owned register snapshot; AArch64 supplies PC, SP, FP
and the interrupted world, while other backends currently report no snapshot.
The PMU overflow handler uses this path, with no profiling observer or PMU
route comparison in common exception entry.

On AArch64, Rust computes the complete guest `HCR_EL2` return regime before
each world switch and passes it through the architecture-private guest-run
frame. Guest entry enables stage 2, selects AArch64 EL1, clears host-only
`TGE`/`DC`, and traps WFI/WFE. A trapped guest wait is therefore a typed vCPU
exit: VM policy snapshots pending virtual interrupts and the architectural
timer deadline, then either re-enters immediately or parks the scheduler-owned
vCPU Thread on its stable endpoint.

Each installed VM has one shared lifecycle aggregate retained independently by
the hardware registry and its handle objects. Its stable vCPU endpoints are the
sole Dormant/Started/terminal state and signal authority; handle inspection and
the scheduler-reaper publication both observe that same state machine. The
aggregate never owns a registry machine lease, so surviving diagnostic handles
cannot prevent hardware ownership from becoming unique for retirement.
VM-specific execution state is type-erased at the task boundary. The scheduler
owns only a stable external-execution allocation, generic resource ownership,
and an opaque detach-completion action; VM endpoint publication remains in the
VM subsystem after the Thread allocation has been destroyed.

The VM lifecycle queue remains the sole Begin -> Quiescing -> Retire owner.
Begin failures and inconsistencies after irreversible retirement cuts fail-stop.
Recoverable retirement preflight failures retain the exact authority and backing
resources, including quarantined devices. The reaper reports each failure cause
once per VM incarnation and reports recovery with the failed-attempt count;
alternating causes do not produce a message on every retry. Diagnostics do not
release quarantined resources or change the retry policy.

The guest virtual-timer PPI is a level source. Injection may mask the host
mapping while a list register owns the pending interrupt; maintenance
reconciliation unmasks it only after the virtual interrupt can no longer be
lost. Because a guest can retire that list register before its timer write has
deasserted the physical level, the existing host tick supplies a bounded
recheck for a source retained masked across that edge. The ordinary tick path
pays only one CPU-local relaxed load while no recovery is pending.

Arm guest SMP admits 1..8 immutable vCPU identities on both GIC backends.
Guest CPU on/off is independent of the frozen host topology: PSCI requests
park or restart preallocated vCPU Threads through runtime-approved power
transitions. A VM-level WaitSet source reports per-vCPU requests and terminal
execution without a userspace thread per vCPU. Guest memory execution claims
are concurrent across host CPUs, while each individual vCPU payload and local
hardware context remain scheduler-owned. Retirement closes run admission and
waits for every claim, CPU context, translation residency, and configured Thread
before releasing backing storage or retiring its software identity. Hardware
VMIDs are separately leased and recycled through rollover epochs. See
[guest power requests and SMP](../design/vm-bundle.md#guest-power-requests-and-smp) for the
Native completion protocol and vm-runtime/vm-manager responsibilities.

Arm virtual GIC active-state registers (`ISACTIVER` and `ICACTIVER`) share
the interrupt model with hardware list-register residency. Local private
accesses synchronize the caller's bank directly. Shared or remote private
accesses temporarily close that VM's guest-entry gate and prompt its resident
vCPUs to save their banks. The register transaction runs after those banks
detach, then entry reopens. This is independent of PSCI power state and VM
administrative stop; unrelated VMs continue to run. Waiting happens only in
scheduler-owned, hardware-detached continuations, including when several guest
CPUs share one physical CPU. Maintenance interrupts and ordinary exits use the
same bank synchronization path; maintenance alone cannot guarantee a fresh
snapshot on demand.

Software-set active interrupts take precedence over pending-only entries when
assigning list registers. Existing active entries are never evicted to admit
another interrupt. Active state beyond hardware LR capacity remains in the
software model. Both `ICACTIVER` and split-EOI `DIR` clear that state, including
interrupts absent from the hardware bank. DIR preserves pending state and, for
GICv2 SGIs, checks the source CPU of the active instance. Shared-SPI DIR uses the
same detached-bank transaction as shared active-register writes.

GICv2 exposes the first CPU-interface page directly and traps the second page
containing DIR. GICv3 traps DIR with `ICH_HCR_EL2.TDIR` when the destination CPU
supports it; otherwise it uses the common-register trap (`TC`). The latter also
emulates PMR, CTLR and RPR from the saved guest VMCR/APRs, never from host ICC
registers. SGI register traps respect the virtual GIC's single-security-state
group rules. This compatibility path costs additional traps; it does not drop
nonresident active interrupts or infer an interrupt ID from maintenance
`EOIcount`, which carries no ID. Ordinary acknowledged active LRs remain resident
until deactivation, so combined-EOI handling remains in hardware.

## Kernel-object ownership

Kernel objects are the standard identity mechanism for service entities with
independent lifetimes or cross-subsystem ownership boundaries. Once an entity
participates in this model, it has one canonical kernel-object allocation.
Kernel code reaches that allocation through direct, counted, typed references;
it does not resolve process-local handle values through a global kernel handle
table. Userspace handle tables wrap the same allocation with process-local
generations, rights, and flags only at the ABI boundary.

Object identity uses a reusable 32-bit slot and a 32-bit generation, encoded
as a nonzero 64-bit KOID with the slot in its low bits. A linear reservation
belongs to the canonical allocation until directory detachment and final
payload destruction finish.
Abandoned construction returns its reservation through the same path. Reuse
advances the generation, wrapping to one at the maximum. Live objects have
distinct identities, but historical IDs may identify replacement objects after
reuse and cannot substitute for counted references. Object-directory
pagination uses a separate registration sequence rather than KOID ordering.
The allocator retains one 16-byte free-list node per reusable slot at the
object high-water mark on 64-bit targets, including outstanding reservations
and deferred finalizers. Active-object storage accounting includes this node;
returned nodes become kernel-owned reusable metadata. Allocation and
deallocation happen outside its IRQ-masked lock, and final release requires no
allocation.

Persistent references declare their ownership class. Scheduler residence,
object-to-object edges, userspace authority, and temporary resolved operations
all contribute to the object's total lifetime count while remaining separately
observable. A borrowed Rust reference is scoped to one counted owner and is not
itself an ownership edge. User-authority count is distinct from total lifetime:
closing the final userspace handle may close an endpoint or publish another
object-specific transition while an already resolved operation safely finishes.
VM device bindings use their own persistent reference class. Such a binding can
be created only by converting a resolved operation pin as part of a successful
consume-on-success handle transaction, and remains separately visible from
ordinary kernel-service ownership until the VM device set is retired.

Reference-class observation is not free: on 64-bit targets the eight counters
occupy 64 bytes per object header. An ordinary counted clone/release updates
both the strong lifetime count and its class counter; directory traversal is
not part of that operation. Keep this cost distinct from business retirement.
An Event needs signal state and accounting, while a VM object additionally owns
an installed-machine reference and publishes stop on last-handle close. Only
the VM lifecycle coordinator performs hardware retirement. Removing diagnostic
classes requires measurement and an alternative ownership-observation contract,
not flattening these two different lifetimes into one count.

The scheduler retains a counted scheduler-class reference to every resident
Thread object. CPU residence and scheduler authority continue to govern access
to mutable scheduling state; an object reference alone never grants that
authority. A user Thread's signals and terminal information remain in its
durable object. System Thread objects currently provide stable identity, while
the scheduler's bounded observations associate that identity with a role and
generation-qualified Thread ID. In both cases, the kernel stack, machine
context, and architecture execution payload are explicitly detached and
retired by the scheduler. A userspace handle can therefore retain a terminated
user Thread tombstone without retaining its execution resources.

Natural Process exit waits for both active Threads and admitted creations to
resolve. If the last active Thread exits while a creation is pending, its exit
status remains provisional: a successful publication replaces it with a new
active Thread, while final rollback completes natural termination. A Process
which has never had an active Thread does not synthesize an exit merely because
creation failed. Explicit stop reasons remain latched independently and take
precedence over a provisional natural exit.

A Process reaching Stopped transfers a counted owner to the kernel reaper.
The reaper closes its handles, waits for outstanding address-space references,
and completes acknowledged address-space retirement before breaking TaskGroup
membership. Retained references and failed machine retirement remain queued;
the reaper arms its preallocated one-shot Callout only before waiting with
pending Process work. A per-CPU timer worker delivers the callback through the
same durable wake protocol as producers. Each batch attempts one Process so
object finalizers and detached Threads can still
make progress; no retry policy is attached to a particular CPU's periodic tick.
Timer cancellation follows its owner queue even after worker migration. Future
CPU hotplug must transfer or drain live timer queues before taking a CPU offline,
as required for all timed waits; this is a timer-subsystem obligation. Process and
Thread termination observations remain durable throughout this cleanup.
Address-space retirement reports its first preflight failure and eventual
recovery with a saturating failure count. It never logs on every delayed poll.

Process handle accounting uses a sidecar with the same segmented slot geometry
as the handle table. Segment growth is fallible, prepared outside Process locks,
and charged before publication. Each slot directly identifies its batch entry;
weak backward links allow an empty batch to be unlinked without scanning the
Process. Native calls run in interruptible Thread context, including allocation
and user copies. A blocking operation suspends its existing kernel continuation.

The final total-reference release performs only an allocation-free, nonblocking
handoff to object reclamation. An object in the reap-pending state cannot be
upgraded from a weak reference, republished as a userspace handle, or otherwise
resurrected. Hardware and subsystem resources must already be quiescent before
this transition; their typed retirement protocols are not delegated to the
generic object reclaimer. Object destruction may release further references,
but does not recursively execute another object's finalizer on the same stack.

KOIDs, object-directory entries, and diagnostic snapshots confer no authority.
The weak global directory can report bounded metadata and aggregate reference
classes, while durable edges are enumerated by their owning Process, scheduler,
or object subsystem. Temporary operation and diagnostic pins are reported as
counts rather than globally allocated edge records. Multi-page graph scans are
weakly consistent and expose neither kernel pointers nor a lookup path from a
KOID to an operational reference.

Userspace observability is represented by immutable `TaskInspector` and
`ObjectInspector` kernel objects. Their payload fixes both a scope (system,
ResourceDomain, TaskGroup, or Process) and a visibility mask. Deriving a view
requires a typed target capability, verifies that the target is already in the
parent scope, and can only remove visibility. Derived non-system views exclude
kernel Threads and global object-directory scans. Process-handle scans accept a
KOID solely as a correlation key after the inspector has authorized the scope;
they never return a handle to the named Process or object.

Kernel-only objects have no generic conversion into userspace handles. Handle
publication requires an explicit typed publication capability; export-policy
flags are diagnostic metadata rather than the security boundary. Kernel
subsystem-owned edges must not form an unbroken strong-reference cycle. An
intentional subsystem cycle therefore has an explicit retirement step which
breaks it; reverse relationships otherwise use weak references or
generation-qualified identifiers.

Userspace can construct cyclic capability graphs, including handles carried by
Channel messages. Reference counting does not collect those cycles: they can
retain resources, but they cannot resurrect an object or create a use-after-free.
Resource accounting and quotas are the current containment mechanism. A future
reclamation policy may add cycle detection or a collector without changing the
typed kernel-reference and userspace-handle boundary.

Implementation records without independent identity remain ordinary subsystem
state. Run-queue links, per-CPU scheduler state, page-table entries, allocator
blocks, architecture register contexts, wait nodes, and device bookkeeping do
not acquire KOIDs merely because they participate in an object's implementation.

## Internal transaction owners

Process handle charge records and their generation-indexed sidecar are owned by
`process::owner::handle_accounting`. Operations run under the existing Process
state lock; Process still decides admission and retirement, and transactions
release detached charges and backing after unlocking. The component adds no
lock or second publication point.

Each CPU scheduler owns a `SwitchHandoff` for its outgoing context and switch-tail
generation. The CPU authority selects the correct owner; generations are local
to that CPU. Completing a stale generation cannot clear a newer outgoing
context. Host tests execute this production mechanism, while runtime scheduler
tests cover queue, migration and architecture integration.

## Native userspace boundary

`entry::user` owns execution sessions, CPU attachment and machine completion.
`entry::services` adapts Native operations using borrowed Process and UserThread
authority; it cannot inspect the private UserSession or retain its return token.
Filesystem adapters have their own child module. One dispatcher routes all
Native calls through ordinary Thread context.

Native user entry follows the same policy-above-mechanism rule without reusing
the vCPU world switch. Complete trap frames, translation registers, return
regimes, and user-context encoding remain architecture-private. The upward
adapter receives an owned syscall invocation and returns a fixed-width result;
it never passes a Rust frame reference into code which can allocate, block,
preempt, or migrate.

The selected `hal::user` facade owns architecture user-address limits, opaque
prepared roots, CPU-local activation/deactivation, and acknowledged local
replacement and invalidation. On AArch64 and RISC-V, synchronous exception entry
passes an owned invocation to the Native service with IRQs masked. The service
enables IRQs and enters preemptible Thread context before dispatch. The private
syscall frame stays on the Thread stack. Calls which do not switch retain their
translation and FP state and return through the vector.
Before an actual context switch, the scheduler withdraws outgoing Native
publication, saves and clears resident FP state, and deactivates translation.
The resumed syscall continues in kernel context; its final return boundary
rechecks stop, pending scheduling, ASID and current mappings before activating
on the current CPU. Lower-user faults and preemption still capture owned state
and unwind the machine run. RISC-V uses private Sv39 roots, a dedicated U-mode
trap vector and full integer/FP state capture. x86-64 currently rejects
native-user entry as unsupported. Process lifetime, handles, rights, object-local
operation policy, bootstrap ELF admission, residency and resource accounting remain in
the kernel. Application ELF parsing, stack encoding and relocation belong to
`bootstrap/stage1/userspace-loader`, `lib/dynamic-loader/elf`, `lib/dynamic-loader` and
the SDK runtime. Descendant process builders explicitly select a loader File;
only initial `/init` uses the kernel's `/lib/userspace-loader-hyper-<arch>` lookup.
The kernel
publishes a bootstrap Thread before ordinary ELF loading; SDK spawn waits for
the userspace startup result. The scheduler-owned `UserExecution` strongly
retains its Process, native address space, and per-Thread machine context.

AArch64 admits IRQ-tail preemption of kernel syscall continuations. RISC-V
retains its existing host IRQ-tail limitation: IRQ delivery remains enabled,
and explicit waits, yields and return-boundary scheduling support continuation
migration, but a host IRQ does not asynchronously preempt the syscall body.

`UserThread` wraps one canonical kernel object rather than maintaining a
parallel task identity. Internal observers and eventual userspace handles share
its KOID, rights ceiling, and level-triggered termination state. Global object
and Process directories retain only weak references. Their cursor-based
snapshots combine object headers with bounded per-Process handle-table pages,
providing a pointer-free diagnostic graph without changing object lifetime or
placing reverse-reference locks on capability hot paths. The Native `handle`
tool renders these snapshots through scoped inspector capabilities. Snapshot queries run in
normal kernel context; fatal diagnostics remain lock-independent.
Process labels are fixed at publication. Native Threads may replace their own
bounded diagnostic labels; inspection copies a complete label under a per-thread
lock, and retirement retains the final name snapshot. This keeps `ps` output meaningful without retaining loader
paths, builder storage, scheduler allocations, or authority-bearing references.

AArch64 keeps the permanent host mapping in the canonical upper range through
`TTBR1_EL2` and gives each Process an immutable lower-range `TTBR0_EL2` root.
Native ASIDs and guest VMIDs use separate rollover namespaces. Old roots remain
retained until every cut target acknowledges; hardware leases cover execution
and tagged maintenance. Safe abandonment leaks published owners rather than
risking premature page reuse. A kernel self-test exercises
repeated direct Native syscall return, register-result validation, blocked
continuation migration with FP/TLS preservation, active COW copyout, contained
fault unwind, join, and retirement on VHE QEMU CPUs. The production path mounts the firmware initramfs during bootstrap,
schedules the `native-init` kernel worker, and retires bootstrap execution.
That worker snapshots `/init`, validates and maps the selected bootstrap
loader, publishes its Process and initial UserThread, and then exits. The
userspace loader validates and maps the application. Runtime file operations
may sleep, so loading requires this ordinary scheduler context. Physical-hardware qualification
remains necessary for guarantees that QEMU cannot establish.

The kernel exposes one Native ABI. Linux currently participates as the trusted
I/O VM described by the project roadmap. Foreign application personalities are
not a delivery commitment and must not impose new requirements on Native paths.

Native object, capability, IPC and ABI contracts are described in the
[userspace and syscall architecture](../design/syscall-abi.md). Reserved image-routing
names do not establish support for foreign application execution.

## Construction and publication

Resources follow `prepare -> validate -> publish`. Construction happens in
local owned state, validation completes before visibility, and a failed
pre-publication step rolls back through ownership or an explicit guard. Each
lifecycle transition has one owner and one publication point.

Kernel objects and userspace handles deliberately have different ownership
interfaces. Kernel subsystems retain compiler-typed, purpose-classified object
references; userspace receives process-local, generation-qualified handles to
the same object identity. Kernel code must not create a private handle table or
perform a handle lookup where a direct typed reference expresses the actual
ownership relationship.

Handle rights and handle flags are separate concepts. Rights authorize named
operations and may only attenuate. Flags modify the behavior of one handle and
are defined by that handle's concrete object type; the raw ABI `u32` is only a
storage and transport representation. A flag bit has no standalone meaning
without the object kind. Object modules own their typed flag vocabulary,
validation, propagation rules, and operational interpretation. Types with no
implemented flag semantics use an explicit empty flag type and reject every
nonzero raw value rather than reserving speculative behavior.

The generic handle core continues to own slot generations, table storage,
accounting, reservations, and atomic publication. An object-specific flag
which affects duplicate, replace, transfer, or another generic transaction
must expose that constraint as a pure, bounded policy evaluated during
preparation. It must not hide allocation, blocking, locking, or fallible work
inside the final commit. Typed SDK bindings associate each object marker with
its flag type; type-erased diagnostics may carry raw bits, but applications
must establish the object kind before interpreting them. The complete object,
rights, flags, and transfer contract is specified in the
[userspace and syscall architecture](../design/syscall-abi.md#typed-handle-flags).

Use `pub(crate)` by default. Add a public interface only for a demonstrated
consumer, and prefer domain types for addresses, identifiers, units, and
lifecycle states when interchange would be unsafe.

Top-level runtime startup invokes one lifecycle entry per subsystem rather
than sequencing that subsystem's internal stages. `kernel::irq::initialize`
publishes host interrupt delivery, `kernel::time::initialize` owns the
clocksource and architectural tick, and `kernel::vm::initialize` activates
hardware virtualization and guest-visible devices. Host timekeeping retains
only the firmware-derived guest-timer source description; VM initialization
owns its host IRQ mapping, handler, rollback, and final publication. Physical-
device setup has two deliberate phases: `device::early_initialize` installs
boot-critical firmware services, while `device::platform_device_initialize`
binds discoverable devices after the core runtime services exist. CPU topology
owns its immutable participation count; timer diagnostics observe that
published state instead of receiving a count forwarded by the kernel entry
path.

The concrete startup order is boot-critical CPU power, memory/allocator,
immutable initramfs publication, debug and scheduler, host IRQ/crash/time,
one-shot SMP admission, stage-1 address-space sealing, platform drivers,
complete VM initialization, and scheduling of the Native init worker. The
worker admits the userspace loader, opens `/init`, and publishes its
bootstrap process after interrupt-masked bootstrap has exited. The in-tree
stage-1 loader then maps static init; it rejects an application interpreter.
Later process builders explicitly select a loader capability. Formal system
programs use the separate dynamic loader, which also accepts static programs.
Kernel self-test images run standalone mechanism tests, report completion and retire the
bootstrap execution. Linux integration boots Native init and uses userspace VMM
tools; kernel self-tests need only an empty ramfs archive. Sealing takes the
same mutation lock as guarded-stack map/unmap and retires identity aliases only
after every admitted CPU has entered permanent high mappings. Its entry rejects
missing `FrozenTopology` before acquiring the mutation lock or changing mappings,
so callers cannot retire trampoline aliases before CPU admission completes.
On AArch64, those
aliases occupy a dedicated lower transition root. Sealing clears every entry
in that root and completes broadcast TLB invalidation; it does not walk firmware
RAM or MMIO intervals and never edits the permanent upper root. The page-table
arena remains owned by the permanent address space; removing aliases does not
reclaim its backing pages. Native Process roots are installed only after this
bootstrap transition is complete.

Host SMP admission publishes a `FrozenTopology` once. Host CPU hotplug is not supported:
late replicated-local transactions snapshot this immutable participant set. A
future hotplug implementation must either join the in-flight snapshot or
replay every live local mapping before publishing a new CPU online.

## Runtime allocation

The central runtime heap is the sole owner of buddy free-block indexes, slab
headers, partial-slab topology, and page-owner accounting. Each buddy order uses
an intrusive binary digital tree keyed by physical block address, bounding
coalescing lookups by address width without allocating separate metadata.
After SMP admission freezes
the participating CPU set, the kernel enables bounded per-CPU magazines for
the smallest slab classes. A cached object remains reserved in its central slab
and is represented by one linear ownership token; its backing page therefore
cannot return to the buddy allocator while any magazine retains an object.

Cache access requires a scheduler pin and local interrupt masking around only
the selected slot mutation. Central heap operations never run while a local
slot lock is held. Cross-CPU deallocation places the object in the freeing
CPU's magazine, and memory-pressure paths may detach all magazines before one
central allocation retry. An explicit reclaim pass supports diagnostics and
pressure recovery but is not a teardown barrier while allocations continue.
The current immutable topology has no offline transition; future CPU hotplug
must quiesce and drain a departing CPU before withdrawing its slot.

## System accounting observations

Runtime accounting remains owned by the subsystem that mutates the underlying
state. The physical allocator produces a copied `MemoryInspector` observation
while holding its existing state lock, then releases that lock before user
memory is accessed. Closed execution intervals update CPU-local nanosecond counters and each
current Thread's monotonic runtime counter. CPU observations first account live
intervals under one CPU lock at a time, then aggregate independent counters.
The result is deliberately weakly consistent across CPUs, not a transaction.

These observations are exposed only through typed, read-only inspector
handles. Applications cannot map or modify accounting storage, and delegating
an inspector follows the ordinary handle rights and transfer rules. Memory
observations contain identities that consumers can validate
(`total = reserved + managed`, `managed = free + used`, and the ownership categories sum to
`used`). CPU categories describe scheduler-entity residency—idle, kernel
Thread, user Thread, and vCPU—not architectural privilege-level cycle counts.
Runtime values are elapsed nanoseconds; there is no configured scheduler tick
frequency. The former frequency word is reserved and reads as zero.

Relaxed atomics are sufficient for CPU-time counters because they convey no
ownership or synchronization state. Each interval changes exactly one monotonic
category, so there is no multi-field publication that a reader could observe
half committed. A future observation containing mutually dependent lock-free
fields must add an explicit sequence counter or immutable snapshot publication
instead of relying on this weaker contract.

## Kernel RPC and replicated-local IRQs

Crash-stop and scheduler reschedule retain dedicated emergency semantics. All
other cross-CPU work shares one Kernel RPC doorbell: AArch64 SGI 8, x86 vector
`0xf1`, or RISC-V SSIP. Per-CPU Release-published reason bits coalesce the
doorbell while typed users retain independent generation/acknowledgement
mailboxes. The acquire-swap dispatcher drains reasons until stable zero and
services stage-1 shootdown before IRQ lifecycle work. Polling progress hooks
drain the same dispatcher without EOI; a real exception entry performs EOI
exactly once. Thus x86 stage-1 shootdown retains its lock-free generation
protocol without consuming another vector or sharing IRQ-administration locks.

Every CPU owns an immutable typed local-controller capability in its per-CPU
slot. RPC work can touch only that CPU's Redistributor, local APIC, or synthetic
local source; it cannot access shared Distributor or IRQ-domain administration
state.

Late IRQ installation is explicitly two phase. `prepare_shared_mapping`
publishes a non-dispatchable handler record and configures the source disabled
on every frozen participant. The owner must quiesce the source and clear any
pending condition first. After publishing all handler dependencies with
Release ordering, `activate` marks the record dispatchable before enabling it
on every CPU. Final-handler removal remains dispatchable until all CPUs
acknowledge disable. A rejected activation is compensated across the complete
original target set. A rejected final disable enters fail-stop because
independently masked local sources prevent reconstructing the exact prior
per-CPU state. Route failure, timeout, generation exhaustion, or failed
compensation is likewise ambiguous and enters fail-stop while the mapping and
handler context remain pinned.

## Scheduling boundary

The scheduler owns stable pinned `Thread` allocations, lifecycle transitions,
per-CPU run queues, placement metadata, elapsed-time accounting, and reschedule
decisions. Replaceable algorithm state lives separately from those mechanisms:
fixed-priority FIFO, weighted-stride Fair (or a configured RR backend), Budget,
and Idle. Native and vCPU execution type does not determine arbitration rank.
Priority-service allowance and a protected Native floor share CPU-local ledgers;
vCPU budgets travel with the Thread. See [scheduling](../design/scheduling.md)
for policy selection, configuration, capability checks, and guarantees.

There is no periodic scheduler tick. A single runnable entity has no scheduling
deadline; a runnable competitor causes the scheduler to publish the next actual
quantum, entitlement, or protection deadline through the one-shot timer service.
Ordinary software timers and guest timers remain independent.

Scheduling synchronization has three domains:

- Each CPU lock owns its current/ready/blocked schedules, run queues, elapsed
  accounting and in-flight context switch. Deadline expiry, yield, preemption,
  current-execution lookup and ordinary switch tails use only that CPU lock.
- Each `WaitQueue` lock owns its FIFO topology. Wait generation arbitration and
  ready publication also hold the waiter's CPU lock. Unrelated CPU/queue pairs
  can park and wake concurrently, including from IRQ context.
- The coordinator owns registry slots, CPU admission, policy changes, migration
  and retirement. Its cache-line-separated reader lanes pin registry lifetime
  and residence: a wait operation takes only its calling CPU's lane. Exclusive
  operations acquire every lane in CPU order before any CPU lock. There is no
  global reader count updated by all CPUs. Writers still exclude wait readers;
  frequent creation, destruction or explicit migration can therefore remain a
  source of contention.

The lock order is condition-object lock, registry reader lane (or all lanes for
coordination), owner CPU lock, then wait-queue lock. A FIFO-head lookup releases
its queue lock before acquiring the owner CPU, then revalidates the head under
both locks. Readers never upgrade or nest. No queue/CPU/registry guard crosses
an assembly context switch; the separate retained IRQ mask still does.

Blocked threads retain CPU residence. Wake-before-context-save can make the
outgoing thread Ready only on its source CPU; its switch ticket still prevents
migration until the incoming tail completes the save. Blocked migration removes
source residence under its CPU lock, releases that lock, and publishes target
residence before ending exclusive coordination. Stack-watermark inspection
checks that a blocked thread is absent from the in-flight context handoff.

Ready membership uses intrusive lists and allocation-free AVL indexes. Algorithm
and service-role buckets expose a bounded set of candidate heads; selection does
not scan every runnable Thread. Policy changes remove a ready Thread before
changing its key and reinsert it under the same CPU lock. Budget epochs refresh
lazily when selection is needed. Fair accounting uses elapsed time divided by
weight, preserves fractional charges and translates virtual debt on migration.
Yield may end a turn but cannot erase execution charges or replenish a budget.
Idle Threads never enter an ordinary run queue.

Ordinary kernel threads carry movable placement policy and may retain an
explicit `CpuMask`. Native and kernel Fair thread creation select a least-loaded
admitted CPU within that mask, preserving creator locality when loads tie.
CPUs without runnable FIFO work are preferred when the mask permits one. A
soft Budget-load penalty prefers space away from runnable vCPUs when comparable
Native placement is available.
Initial vCPU topology placement spreads a VM's members across admitted CPUs;
FIFO creation retains its existing creator preference. Empty masks and masks
with no registered CPU are rejected. Explicit migration and affinity updates move
dormant, ready, and fully stopped blocked movable threads synchronously under the
exclusive coordinator. A running or switch-in-flight thread retains the request
on its own `Thread`; the source CPU commits a switch and the incoming switch tail
publishes target membership only after assembly has saved the complete source
context. Native user runs detach their translation root before scheduling;
vCPU IRQ tails save and detach virtual hardware, active execution publication,
and Stage-2 residency before this same handoff. Resumption reacquires execution
on the destination CPU, synchronizes migrated guest instructions, and restores
virtual interrupt and timer state. WFI and MMIO waits retain durable work while
their detached threads move. The public vCPU control is `set_affinity`: keeping
its assigned CPU in the mask preserves placement; excluding it selects an
eligible CPU and triggers this handoff. The scheduler Thread owns the mask,
which survives guest CPU off/on cycles. No separate public migrate operation
or duplicate vCPU affinity state is needed. Bootstrap, idle and CPU-owned
timer workers remain pinned. Their names include the CPU (`bootstrap/0`,
`idle/CPU`, `ktimer/CPU`); `kthread_create_per_cpu` fixes both naming and placement
for additional CPU-owned workers. Such threads reject explicit affinity changes
and migration too. Global workers retain ordinary movable placement and no CPU
suffix. A multi-CPU affinity mask permits later automatic runnable balancing; use
a singleton mask when a thread or vCPU must remain on one CPU.

Automatic balancing runs at scheduling and readiness events. There is no
balancing Thread or periodic balancing timer. A pass examines at most 64 registry
slots and migrates at most four ready Threads. A finite sweep captures the
registry extent and uses coalesced continuation prompts to reach later candidates;
new events do not restart an unfinished sweep. It never waits for the exclusive
coordinator: a failed try-lock subscribes to the blocking lane's actual release.
Entering Idle requests work, while remaining idle does not repeatedly rescan.
Destination CPUs are prompted only after scheduler locks are released.

Ready Fair and Budget Threads with movable placement are candidates. Affinity,
CPU-local wait constraints, pending explicit migrations and source-context
handoffs remain authoritative. Running, blocked, dormant, pinned, FIFO and Idle
Threads are not moved automatically. Load includes the caller and counts actual
runnable work, excluding a vCPU parked in WFI. A destination must have no runnable
FIFO work and at least two fewer runnable Threads before the transfer. A soft
Budget-load penalty breaks destination choices; at most one automatic migration
is allowed before the moved Thread actually executes. Runtime debt and remaining
budget follow the Thread without refilling.

This policy provides neither admission-controlled real-time guarantees, NUMA or
heterogeneous capacity placement, nor aggregate per-VM CPU quotas. It does not
force a running vCPU to migrate, and a busy coordinator or strict FIFO work can
delay convergence. Initial placement does not count dormant Threads: a batch
created before any member starts can require later event-driven repair.

The kernel always builds the SMP-capable scheduler and per-CPU infrastructure;
the same image remains valid when firmware admits only the boot CPU. There is
no separate uniprocessor configuration or single-CPU scheduler implementation.
Run queues store stable `ThreadId` values and do not own Thread allocations, so
CPU placement changes do not move object ownership.
Exited threads enter a scheduler-owned intrusive reclamation queue. Reaping
therefore scales with pending exits rather than the lifetime `ThreadId` space,
and releases an allocation only after no CPU retains it as a current or
switching-from thread.

Every `Thread` embeds one generation-tagged wait record. Its
`Idle -> Armed -> Queued -> Completed -> Idle` transaction and intrusive queue
links are updated under its owner CPU lock and the affected queue lock, so
notification, timeout, and cancellation select exactly one terminal
`WaitOutcome`. Exclusive lifecycle operations use the same CPU authority while
excluding reader lanes. A resolver which
arrives while the wait is only Armed records its result for the caller instead
of losing the event; a stale generation cannot resolve a later wait on the same
queue. Queueing and wakeup allocate no memory. Condition-based primitives hold
their own IRQ-masking state lock across the condition check and queue
publication; Mutex and Semaphore preserve direct handoff, while Completion
provides counted and permanent-complete states.

Timed waits allocate their timer and callback owner before queue publication,
then use the same wait transaction. The callback context remains heap-stable
until cancellation succeeds or a release/acquire handshake proves that an
already-detached callback has returned. A timer stays on the CPU which armed
it even if the blocked Thread migrates. Early notification or cancellation
uses the handle's queue identity to retire it from that source CPU; remote
retirement of these IRQ timers deliberately leaves an obsolete earlier
comparator programmed and therefore causes at most one harmless timer interrupt
before the source programs its next deadline. Sleep is policy over this common
timed-wait path.
Ordinary kernel callbacks use typed `Callout` owners and per-CPU `ktimer/CPU`
Threads; `IrqCallout` and the `schedule_irq_*` interfaces explicitly retain
hard-IRQ execution for critical paths. Both use the same allocation-free,
augmented red-black deadline queue. Callout shutdown closes rearm admission and
joins execution before callback context destruction. See
[Kernel timers and callouts](../design/timers.md) for execution contexts, cancellation,
clock ordering, cross-CPU prompting, and validation.
Waits are migratable by default. A blocker retaining genuine CPU-local state
must arm with `CpuLocal` mobility, which rejects reassignment in both Armed and
Queued phases rather than relying on the generic `Blocked` state.

Per-CPU preemption state coalesces class, quantum, and remote-wakeup requests,
tracks explicit disable guards and IRQ nesting, and is online before the local
timer can deliver interrupts. The pending bit is the durable scheduling
condition, not the IPI: Release publication follows ready-queue publication,
Acquire observation precedes the scheduling decision, and consumption occurs
only under the target CPU scheduler lock. Its `false -> true` publisher owns target
notification; later publishers coalesce until the target consumes the bit.
When a request is made inside an interrupt on its owning CPU, the active
outermost IRQ already supplies the notification and avoids a redundant
self-IPI. Remote callers never inspect target-local IRQ depth.

A targeted reschedule interrupt is only a prompt to evaluate the pending bit.
Its permanent kernel handler does not schedule or own scheduler state;
controller completion and outermost IRQ accounting precede the scheduling
decision. AArch64 qualifies this complete IRQ-tail seam and consumes requests
on the interrupted Thread's stack. An interrupted vCPU is fully deactivated
before the scheduler switch and reactivated only when that same continuation
resumes. `cond_resched` provides the corresponding cooperative safe point.
Logical CPU routing is validated before scheduler registration. An interrupt
sent during secondary bring-up may precede physical online state, but the
secondary first installs and validates its CPU-local vector register (VBAR_EL2,
STVEC, or IDTR), then initializes local IRQ and timer state. Its first
scheduler-locked idle observation publishes online only after the fallible
queue check succeeds, so the boot CPU cannot reclaim the handoff or enqueue
work into an admitted CPU which has not completed exception entry setup.
Runtime CPU offline and hotplug remain unsupported and may not assume that
bootstrap exception.

Idle entry uses one interrupt-mask ownership interval for its final run-queue
check and architecture wait. RISC-V executes WFI with global delivery masked;
a locally enabled pending interrupt resumes the hart and remains pending until
the saved mask is restored. AArch64 temporarily admits IRQ delivery around WFI
because a masked physical PPI is not required to retire WFI on every
implementation. If an IRQ publishes work in that enable-to-WFI window, the
qualified outermost IRQ tail switches away from the idle Thread; this
continuation can reach WFI only after idle runs again, and it reestablishes the
outer guard's masked state before returning. x86 uses `STI; HLT; CLI`, whose
interrupt shadow makes the enable-and-sleep boundary atomic. Each backend thus
closes its own queue-check-to-sleep race without assuming a cross-architecture
masked-wait behavior.

Every scheduling transition masks interrupts before the scheduler publishes
`current = next`. Blocking primitives transfer the outer synchronization lock's
mask into their park token, so releasing that lock cannot expose a logical/
machine owner mismatch. At the final infallible boundary the transition
consumes the CPU-affine guard and supplies its exact saved state to assembly;
the guard itself never survives on a suspended stack. Assembly saves that state
in the outgoing `ThreadContext`, changes to the incoming stack, and invokes a
common completion callback while interrupts remain masked. The callback retires
source `switching_from` ownership and applies any Thread-owned migration request
before assembly restores incoming DAIF, SSTATUS.SIE, or RFLAGS.IF. Passing raw
context pointers across this boundary avoids retaining Rust references while
the callback re-enters scheduler ownership.

RISC-V Native U-mode execution supports asynchronous preemption through its
owned interrupted-run return. Its guest IRQ return also qualifies the
deactivation and IRQ-tail switching contract. RISC-V host continuations and
x86-64 retain cooperative scheduling. Both can send a targeted wake prompt—an
SBI software interrupt on RISC-V and a fixed x2APIC IPI on x86-64—while retaining
host preemption requests for a later cooperative point. Each architecture and
execution origin must qualify the IRQ-tail boundary independently rather than
inheriting the AArch64 capability through a misleading common interface.

## Logging and diagnostic identity

The kernel log ring timestamps a record when it is produced, rather than when
the asynchronous console drain observes it. Architecture assembly captures a
boot counter before Rust entry; once the clocksource publishes its frequency,
records use boot-relative seconds with microsecond precision. Records emitted
before that publication deliberately use zero. Ring sequence numbers remain
internal cursor and loss-detection state, while console output reports an
explicit warning only when records were missed.

`CONFIG_LOG_COMPILE_LEVEL` sets the maximum severity present in the kernel
image. Statically leveled callsites above that ceiling have no generated code,
argument evaluation, or ring-buffer side effect. Independently,
`CONFIG_CONSOLE_LOGLEVEL_DEFAULT` initializes the mutable runtime Console
threshold. Console filtering never removes an otherwise compiled record from
the ring; it only decides whether the drain worker presents that record to the
normal Console sink.

Thread diagnostics copy names into fixed-capacity owned snapshots while the
scheduler owns the source Thread. Logging and crash paths can therefore render
an ID/name pair without retaining a `Thread` reference or reacquiring object
ownership after the scheduler boundary.

## Bootstrap boundary

Architecture assembly transfers the three machine-word boot arguments to the
kernel-owned `hyper_bootstrap` entry. That entry calls
`hal::platform::prepare_boot`, which performs architecture preparation and
returns typed `ProtocolInputs`; kernel code then applies boot policy. Secondary
CPU entry likewise transfers through an explicit kernel callback. The assembly
linkage is an explicit executable entry contract, not a Rust dependency on
kernel internals.

Ordinary exception, interrupt, VM-exit, failure, and virtualization mechanisms
use immutable registered services. Raw frames and register manipulation remain
inside the HAL crate. The binary owns final linking and boot policy; shared
Kconfig parsing in `build_support/config.rs` keeps configuration definitions
consistent across the binary, core, and HAL builds.

### Unrecoverable invariant failures

An internal invariant that has already failed must report through the fatal
crash path, never through an unannotated permanent spin loop. Reusable
mechanisms use `hyper::debug::invariant_failure`; the binary panic handler
provides the kernel policy. The kernel uses `panic=abort`, so live execution
claims and other ownership guards are not unwound or released after corruption.
The failure path must not allocate or use ordinary logging or kernel locks.
Once crash handling is ready, it captures the reason, coordinates CPU stopping,
and emits bounded emergency diagnostics. Before safe console initialization,
it retains a debugger-visible record when CPU identity is available; serial
output cannot be guaranteed before its hardware prerequisites exist.

This requirement does not prohibit busy waits, CAS retries, scheduler idle
loops, or the final CPU halt after crash handling. Those have different
progress or terminal-state contracts.

### Translation identifiers and rollover

AArch64 selects ASID and VMID widths independently from
`ID_AA64MMFR0_EL1.ASIDBits` and `ID_AA64MMFR1_EL1.VMIDBits` during boot.
Hardware supporting 16 bits uses `TCR_EL2.AS` for VHE host translations and
`VTCR_EL2.VS` for guest translations; 8-bit implementations remain supported.
FEAT_VHE alone does not imply either 16-bit capability. The immutable selection
is checked on secondary CPUs before admission; the stage-1 ASID requirement is
also checked before installing their TCR.

Hardware bindings use the transparent `TranslationTag(u64)` type. A validated
namespace width determines the layout: low 8 or 16 bits contain the hardware ID,
and the upper 56 or 48 bits contain the allocation epoch (other admitted widths
use the same rule). Encoding and decoding stay inside the tag/lease types;
callers do not shift or mask raw values. Hardware receives only the decoded ID.
The packed tag grants no execution authority without its retained lease.

ASID and VMID namespaces use independent rollover epochs. Software address-space
identity is a monotonic 64-bit serial, not a permanent hardware tag. Creating an
address space does not consume one of the 255 nonzero tags on 8-bit hardware.
A run acquires a hardware lease before publishing execution; concurrent vCPUs
of one VM share the same binding. The lease pins that tag until local hardware
is detached. Live page-table maintenance also retains a lease through its last
invalidation acknowledgement.

When unused tags run out, the namespace advances its epoch and makes unpinned
tags reusable. Pinned tags retain their owners across any number of rollovers.
Before a CPU installs a lease from an unseen epoch, the HAL completes a local
namespace-wide TLB invalidation. This avoids a synchronous cross-CPU rendezvous
under the namespace lock. Mapping and instruction-cache publication epochs are
separate and remain unchanged by tag rollover. A stale cached binding is only a
hint; the pool validates its owner before reuse.

An approximately 2 KiB directory indexes up to 256 optional segments, each
covering 256 tags. Per-segment bitmaps select unused tags; each software owner
caches its current binding, avoiding a namespace scan on the execution path.
Metadata is prepared when software owners are registered, outside the namespace
lock; acquiring a run lease never allocates. Hardware width fixes the namespace
limit: 8 bits admits tags 1..255 and 16 bits admits tags 1..65535. RISC-V uses its
independently probed ASID and VMID widths. RISC-V implementations with fewer
than eight VMID bits use an eight-bit logical namespace with hardware VMID zero;
they flush all guest translations on every admission instead of relying on
hardware tags. This also covers implementations without VMID bits.

Unregistering an owner leaves a used-tag tombstone until the next epoch, so it
cannot enable reuse before CPUs flush. Excess unpinned segments can be detached;
this also advances the epoch before removing reuse history. Memory is freed
outside the namespace lock. Neither software serial nor epoch wraps: the epoch is bounded by the remaining
`64 - width` bits, and overflow fails without publishing a replacement identity
or recycling live tags.

A fully pinned namespace reports temporary `Busy`. Native admission handles
this before installing an active translation and yields before retrying.
A syscall returning after suspension uses the same admission rule. Guest execution cannot saturate the production VMID namespace: at
most 64 installed or retiring VMs can hold leases, each uses one tag regardless
of vCPU count, and the HAL provides at least 255 nonzero logical tags, using the untagged
RISC-V fallback where necessary.
Construction-only reservations do not pin a tag. This bound must be revisited
if the VM registry limit or supported hardware widths change.

Final retirement still closes admission and waits for every resident CPU to
acknowledge the end of hardware access before freeing page tables or backing.
It uses stable software identity and conservative local invalidation, without
claiming a new hardware tag merely to destroy an inactive address space.

## Floating-point state

AArch64 and RISC-V use soft-float kernels with run-scoped lazy restoration for
Native threads. AArch64 guests are also lazy; RISC-V guests restore once per run
to retain the supported platform's normal illegal-instruction delegation. [Floating-point context ownership](../design/floating-point.md)
describes first-use traps, scheduling and migration boundaries, and the linked
image audit. Native application ABIs remain hard-float. This avoids unconditional
FP copying on direct exception returns; it does not promise a performance gain
for every workload, since kernel memory routines also use general registers.
