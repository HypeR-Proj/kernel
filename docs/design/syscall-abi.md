<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Userspace and syscall architecture

This document describes the Native userspace boundary, ownership contracts,
and syscall execution model. Current object interfaces and signatures are defined by the
compiler-checked schema in `sdk/abi`; exploratory interfaces below are labelled
as planned. Linux/FreeBSD application compatibility is outside this contract.

The current integration goal is Native applications plus a trusted Linux I/O VM
on Pi 5; see the [roadmap](https://github.com/HypeR-Proj/HypeR/blob/main/docs/project/roadmap.md). HypeR is pre-release, so operation selectors and binary layouts do not yet carry a release compatibility guarantee.

For operation grouping and consolidation boundaries, see
[Native object interfaces](syscall-families.md).

## Design goals

- Keep the EL2 kernel small enough to audit while exposing enough mechanism for
  a real VMM, loader, and service manager.
- Make authority explicit and monotonically decreasing. Integer identifiers,
  object identity, and diagnostic access are never authority.
- Make every capability creation, transfer, publication, and revocation an
  all-or-nothing ownership transaction.
- Keep architecture frames and register layouts private while publishing a
  stable, fixed-width machine ABI.
- Support direct native syscalls. A vDSO is an optimization and compatibility
  surface, not a security gate.
- Require FEAT_VHE for AArch64 hosts and reject unsupported CPUs before
  establishing the host translation regime. An Arm version alone is not admission.
- Reuse the scheduler's existing wait, timeout, cancellation, affinity, and
  migration contracts.

The first implementation is not required to implement the complete target
surface. It must, however, preserve the boundaries and invariants in this
document from its first syscall.

## System model

The kernel exposes one Native ABI. Native applications, the VMM, and system
services consume it directly or through the SDK and Rust standard library.

```text
Native applications, VMM and system services
                      |
              Native syscall adapter
              capability validation
                      |
    Process, memory, IPC, wait, task and VM services
                      |
       selected HAL and architecture mechanisms
```

Linux runs as an I/O backend VM.
The near-term Pi 5 target uses a trusted Linux VM driving physical network and
storage devices. Native services communicate through bounded shared-memory
queues, explicit buffer grants and event/session lifecycles. HypeR-owned RAM is
excluded from Linux's allocatable memory; this reservation does not provide DMA
isolation. Device-visible memory cannot be released until DMA is quiescent.
IOMMU-backed leases are a later prerequisite for untrusted device domains, not
for this trusted bring-up. See the [current roadmap](https://github.com/HypeR-Proj/HypeR/blob/main/docs/project/roadmap.md).

## vCPU affinity

`virtual_cpu_set_affinity` borrows a `VirtualCpu` with `WRITE` and accepts the
same bounded little-endian `u64` CPU-mask words as process-builder affinity.
The nonempty mask must include an allowed registered CPU; bits above the kernel
CPU limit are rejected. If the current CPU remains allowed, placement is kept.
Otherwise the scheduler chooses an allowed CPU and safely migrates the saved
context. Automatic runnable balancing may subsequently move a Ready vCPU within
the allowed set; a singleton mask keeps it on the selected CPU.

Success means the update or required source-context handoff was accepted; the
handoff may finish before or after return. Stopping VMs and retired vCPUs reject
updates; an incompatible in-flight update or CPU-local wait returns `busy`.
The stable scheduler Thread retains affinity across guest CPU off/on.
`virtual_cpu_get_info` retains its 24-byte prefix and appends `host_cpu` and
`migration_target`. These fields are one placement snapshot, not running state
or a completion token. `0xffffffff` denotes unavailable assignment or no pending
destination. SDK bindings handle shorter records from older kernels.

## Trust and containment

The design assumes that any EL0 caller can:

- invoke arbitrary target handles and operation selectors with arbitrary register values;
- race handle operations, mappings, waits, cancellation, and process exit on
  multiple CPUs;
- mutate or unmap user memory during validation;
- exhaust handles, messages, pages, timers, wait registrations, and kernel
  metadata;
- forge object identifiers, stale handle values, and ABI metadata.

The kernel, selected HAL, architecture implementation, and boot trust chain are
inside the trusted computing base. Native callers remain confined to their
explicit capabilities, mappings and accounting budgets. The current Linux I/O
VM is also trusted: stage-2 restricts CPU accesses, but reserved host memory is
not DMA isolation. An untrusted driver domain requires separately qualified
IOMMU-backed ownership and retirement.

Speculative state, extended registers, debug state, TLS, and address-space
identifiers are part of the execution boundary. AArch64 and RISC-V use the
[run-scoped FP ownership contract](floating-point.md): first use restores the
exact admitted Native owner's initialized state, while every ownership boundary
saves and clears the physical bank before another context can run. Kernel
code cannot borrow that bank. QEMU probes validate these state transitions;
physical trap and speculative-execution behavior still needs hardware
qualification. Cross-run lazy ownership caching is not implemented.

## Layer and module boundaries

The intended production layout is:

```text
sdk/abi/                    compiler-checked schema and public interfaces

src/entry/user/mod.rs    one upward user-entry adapter
src/abi/             ABI values and Native dispatch
src/process/         process image, handle table, user threads
src/capability/      object header, rights, handles, transactions
src/ipc/             channels, events, wait sets, backend sessions
src/mm/user_space/   VMO, VMAR, mappings, safe user-copy ownership

hal/src/hal/user.rs         selected user-world machine capability
hal/src/arch/*/user.rs       private frames, roots, registers, entry and return
```

Dependencies flow downward. `kernel::abi` contains fixed-width values and
dispatch policy but no architecture register offsets. `kernel::capability`
does not depend on Channel or VM policy. IPC composes capability transactions;
object-specific behavior remains in its owning subsystem. HAL knows address-
space activation and user entry mechanisms but never Process, object operation selectors,
rights, or ELF metadata.

The earlier `UserExecution { AddressSpaceId, UserContext }` placeholder was
removed because it could not pin mappings or authorize activation. A runnable
user Thread must retain a strong Process and prepared-address-space capability.
An identity remains useful for diagnostics and TLB tagging, but is not
authority.

## Process image

Each installed `ProcessImage` owns immutable machine ABI, initial entry/stack/TLS
values, and bootstrap mapping bounds. These describe the userspace loader and
temporary stack, not the final application entry or its auxv. The address space
is owned separately by `Process`; mutable registers belong to the Thread.

Only Native images can be constructed. Before creating a user Thread, `Process`
checks that the image's machine matches the host. Loader metadata and ELF branding
are untrusted inputs, never sources of authority. The Native ABI revision remains
zero during pre-release development and is not stored in individual images.

Process construction and publication use the builder transaction. In-place image
replacement and foreign ABI execution are not implemented.

### Native process startup

The kernel maps only a caller-selected restricted loader ELF and an empty
guarded temporary stack. Initial boot alone opens
`/lib/userspace-loader-hyper-<arch>` to start `/init` from the RAM archive root.
The first argument register carries a ByteChannel handle whose queued message
contains `loader_startup`, tagged startup handles, and opaque startup data.
The 96-byte fixed record supplies an executable File, mapping capabilities and
their actual bounds, plus ExecutionContext, ObjectFactory and SystemInfo handles.
All handles already belong to the child's table. The first channel read uses its
explicit receiver; no implicit context is needed to obtain the startup record. The
[generated ABI reference](https://github.com/HypeR-Proj/abi/blob/main/docs/native.md) defines record layouts,
rights and syscall result slots.

`process_builder_set_data` replaces up to 16 KiB of opaque data; an empty input
clears it. The kernel validates the bounds and copies the bytes, without parsing
arguments or environment entries. Their encoding and limits belong to the SDK.
Builder creation derives a child File capability from explicit
`READ|EXECUTE|PROTECT|DUPLICATE|TRANSFER` authority, attenuating the child grant
to `READ|EXECUTE|PROTECT`. The selected loader requires `READ|EXECUTE`; its
restricted image is captured only for kernel bootstrap preparation. The target
File remains mutable until the child loader acquires content protection.
Sealing prepares the bootstrap environment. Neither validates the application's ELF or resolves
dependencies.

`process_builder_start` consumes the sealed builder only on success and returns
a supervisor Process in `value0` plus a READ/WAIT ByteChannel in `value1`.
Success means the child has been published and its bootstrap can run. A syscall
failure leaves the builder available; later userspace loading failures cannot
restore it. The SDK waits on its startup-result protocol and, on failure,
requests child stop and waits for termination. A queued successful result is
read before interpreting peer closure, allowing short-lived children to spawn
successfully. Readiness precedes constructors and `main`.

Init uses the same bootstrap with empty startup data and no reply request.
Application ELF parsing, initial `argc`/`argv`/`envp`/auxv, relocation, final
stack placement and heap policy belong to the loaders and runtime. See the
[Native init contract](native-init.md#executable-image) and
[bootstrap loader](https://github.com/HypeR-Proj/bootstrap/blob/main/userspace-loader/README.md) and
[formal loader](https://github.com/HypeR-Proj/lib-dynamic-loader/blob/main/README.md) for their entry policies.

## Entry and completion contract

Complete trap frames remain architecture-private. Architecture-neutral code
never borrows a raw exception frame and never learns its field offsets.

For Native calls, the architecture entry adapter:

1. proves that the current Thread owns an active user execution context;
2. copies the target handle, object-local operation, six business argument words
   and call site into an owned `NativeInvocation`;
3. passes no private-frame reference or architecture offset to policy;
4. invokes the borrowed `kernel::entry::user` service; and
5. encodes its `NativeResult` and returns through the vector, or captures a
   terminal context when Thread or Process stop forbids user return.

Every syscall runs on its Thread's kernel stack with normal IRQ delivery and
preemption enabled. The single dispatcher handles queries, user copies,
allocation, IPC and blocking operations. Invalid or stale target handles return
`BAD_HANDLE`; an unknown operation on a valid receiver returns `NOT_SUPPORTED`. A syscall which completes without a
context switch retains its active translation and resident user FP bank. The
kernel itself keeps FP access disabled. User copies may populate file pages or
resolve COW through the normal mapping transaction and acknowledged replacement
protocol while that Process is active.

A blocking call suspends this same kernel continuation. Before an actual
scheduler switch, the outgoing Thread withdraws its CPU-local Native
publication, saves and clears any resident user FP state, and leaves its active
translation. This occurs before another CPU may run the Thread. After waking,
the syscall continues under ordinary kernel translation, and may sleep again.
The final return boundary checks stop and pending scheduling work, then acquires
an ASID and activates the current mapping epoch on the then-current CPU if the
call was suspended. The no-switch path needs no repeated root activation.
No CPU-affine guard survives migration.

AArch64 permits kernel IRQ-tail preemption during the syscall. RISC-V retains
its existing host IRQ-tail limitation and schedules at explicit waits, yields,
and the final return boundary; its syscall body still admits normal IRQ delivery.

The Process and UserThread authority, logical run generation, and
architecture-private syscall frame belong to the stable Thread continuation.
Scheduling does not expose that frame to policy or complete the syscall twice.
Exit and cancellation suppress user return even if a service has already
produced a result. Fixed-width result encoding is infallible; a syscall which
published capabilities never attempts rollback merely because stop arrived
before user return.

Lower-user faults and preemption still capture owned state and unwind the
machine run. Their return capability is linear and bound to the exact stopped
`UserContext`, Thread, ProcessImage generation, and run generation. The stopped
run remains occupied until the caller resumes or discards that capability.
Dropping an armed return owner enters a lock-, allocation-, and diagnostic-free
fail-stop.

Architecture entry does not interpret target handles, operation selectors or
process policy. Kernel services return typed internal errors; the Native adapter maps
them to `NativeStatus`.

## Native machine ABI

Every Native trap invokes a process-local capability. The register previously
used for a syscall number carries the receiver handle; a separate register
carries an operation local to that receiver's object kind. The six argument
registers contain business arguments only, without repeating the receiver.
Every operation leaves at least one register reserved and zero. Sized request
records and output strides are specified in the
[Native ABI reference](https://github.com/HypeR-Proj/abi/blob/main/docs/native.md).

| Architecture | Receiver handle | Operation | Arguments | Result |
| --- | --- | --- | --- | --- |
| AArch64 | `x8` | `x6` | `x0`–`x5` | status in `x0`, values in `x1` and `x2` |
| RISC-V 64 | `a7` | `a6` | `a0`–`a5` | status in `a0`, values in `a1` and `a2` |

x86-64 Native entry remains unimplemented; no x86 register contract is promised.
Common handle/object operations occupy selectors 0–5. Kind-specific operations
start at 256 and may reuse the same numeric selector on different kinds. Zero
is not a valid target handle. Unused argument registers must be zero.

Admission captures the receiver's generation, kind, rights and internal lifetime
pin under the Process handle-table lock, then releases that lock before dispatch.
The operation decoder and rights checks use this captured authority. Normal
operations do not repeat receiver lookup. Operations on the handle namespace
(close, duplicate, replace, and supervisor-to-observer derivation) still require
the original live slot generation; consuming transactions also validate object
identity at commit. An admitted object operation may finish after a concurrent
close, including publishing its declared result handles. Its internal lifetime
pin cannot duplicate or revive the closed receiver's handle authority.

Operations without a natural object receiver use real startup capabilities:

- ExecutionContext controls the caller's execution, current Thread operations
  and process-local waits. Its rights can be reduced independently. Waiting or
  sleeping requires `WRITE` on this context; it has no signal source and does
  not grant `WAIT`, which authorizes observing another object's signals.
- ObjectFactory creates local objects charged to the caller's ResourceDomain.
- SystemInfo permits read-only ABI, configuration and clock queries.

ExecutionContext and ObjectFactory are bound to the creating Process and cannot
be transferred. They hold its identity, not an owning Process reference.
SystemInfo is transferable read authority. Closing or attenuating these handles
has ordinary capability semantics; there is no selector that recreates ambient
authority. SDK wrappers retain the startup handles for convenient typed calls.
Runtime getters return borrowed handles: callers must not close or replace
them. A raw close or replace invalidates the retained generation; the runtime
does not rebind or recreate that authority, so subsequent context-dependent SDK
calls return a handle error (the no-return exit wrappers trap). To invoke with
reduced rights, duplicate a separate attenuated handle and pass it explicitly to
`hyper_native_invoke6`.

`NativeResult { status, value0, value1 }` returns up to two new handles without
a fallible user-memory copy. Failure values are zero except where the schema
explicitly permits required-size or partial-transfer information. This policy is
selected by receiver kind and operation together. Larger data uses explicit
output buffers.

ABI values use fixed-width integers. Public layouts never contain `usize`, Rust
references, Rust enum layout, implicit bitfields, or an unqualified `bool`.
Structures have C layout, explicit padding, reserved-zero fields, and checked
size and offset assertions. Extensible records have an explicit size only when
older and newer layouts can be interpreted safely. Unknown strict flags are
rejected; a field is flexible only when its schema says so.

The direct trap ABI is implemented first. A future vDSO will provide recommended
C-compatible wrappers and optimized time or shared-page operations, but the
kernel will never check that a syscall instruction originated there. Each
facility defines its own shared-page layout and discovery contract.

Native blocking calls use absolute monotonic deadlines and an explicit infinite
sentinel. They report cancellation rather than silently rewinding the PC.

## Declarative ABI schema

One compiler-checked Rust data schema under `sdk/abi/` is the source of truth.
The kernel consumes its dependency-free `no_std` crate through an in-tree path
dependency without maintaining a second ABI definition. Released Native SDKs
are generated from the same repository commit as the kernel that implements
their contract.

Each invocation declaration records at least:

- receiver object kind, local operation selector, public name and feature gate;
- ordered argument and result shapes;
- handle object kind, required rights, consume/borrow/produce semantics, and
  resulting rights;
- user-memory direction, linked length, maximum size, and observable
  validation order;
- blocking, cancellation, restart, no-return, and audit classes; and
- strict or flexible flag behavior.

The current generator produces Rust values, a C constants-and-layout header,
invocation metadata, layout assertions, an ABI reference, and compatibility tests.
It is intended to generate kernel dispatch wrappers, architecture stubs, and
vDSO exports as those layers are introduced. Generated source is checked into
the SDK-facing locations where appropriate; CI regenerates it and rejects
drift. Semantic validation remains in named handlers and is not hidden inside
an unreviewable generated wrapper.

The ABI revision field is reserved process-image metadata. Its value remains
zero throughout pre-release development: schema changes do not increment it.
ABI versioning and compatibility policy begin only after an explicit project
decision to publish a supported userspace ABI.

## Objects, handles, and rights

An object has a 64-bit `Koid` for diagnostics and tracing. Its low 32 bits hold
a nonzero reusable slot; its high 32 bits hold a generation. A slot is returned
only after final payload destruction; reuse advances its generation, wrapping
to one after the maximum. KOIDs are unique among live objects, but a complete
value can repeat after destruction. Neither a KOID nor its slot can open an
object or authorize an operation. Diagnostic queries describe the current
object and cannot establish identity across destruction and reuse. Consumers
must not infer creation order from an ID. Language runtimes with a 32-bit process-ID surface may expose
the slot using `HYPER_NATIVE_KOID_SLOT_MASK`; that value is unique while the
object remains alive and reusable afterward. Native interfaces retain the full
KOID, and process operations continue to require handles.

Every owning object reference shares one private erased allocation, but its
Rust type records both the payload type and ownership class. The sealed
`KernelObject` trait requires `Send + Sync + Any`; concrete operations remain
inherent methods on concrete types. Handle lookup validates the kind before a
safe `Any` downcast creates an operation-pin reference. An object-kind tag is
never followed by an unchecked pointer cast. One generated declaration keeps
Rust types and ABI object kinds unique and coherent.

Object construction uses the shared `FallibleArc` allocation boundary and
returns allocation failure before publication. Untrusted creation paths may
not replace this with an infallible shared-owner constructor or duplicate the
reference-count implementation.

Constructed objects also enter a global diagnostic directory through a weak
reference. The directory never grants authority or retains the payload. Its
newest-first cursor excludes later registrations from subsequent pages, while
each snapshot reports KOID, kind, immutable export policy, active-handle state,
and weakly consistent counts for service, scheduler, publication,
user-authority, operation-pin, diagnostic, retirement, and VM-device-binding
owners. Published
Processes have a corresponding weak directory, and each Process exposes
bounded pages of generation-qualified `handle -> KOID` edges. Native
`TaskInspector` and `ObjectInspector` objects turn those mechanisms into
explicit observation capabilities. A system inspector may be monotonically
derived into a ResourceDomain, TaskGroup, or Process view only by presenting a
typed target handle; numeric identifiers never create a view or an operational
handle. An out-of-scope target is indistinguishable from an absent target.
Multi-page results are weakly consistent with mutation, use fixed ABI page
capacities, and expose neither a pointer nor an authority-bearing identifier.
Task records also carry bounded UTF-8 diagnostic names. A Process snapshot keeps
the label committed with Process publication, including after its heavyweight
owner is gone; the scheduler similarly retains each Thread label through the
retiring registry phase. `thread_set_current_name` lets a Native thread replace
only its own label with 1–64 UTF-8 bytes containing no control characters. It
needs no additional capability, cannot rename another thread or its Process,
and leaves the old label intact on failure. Inspectors receive a complete old
or new label under a small per-thread lock; user copying occurs before that
lock, with no allocation or scheduler state change inside it. Numeric phases,
roles, kinds, and rights remain the canonical ABI facts, while SDKs and
diagnostic tools provide human-readable interpretation.

A Native handle is a process-local `u64` whose most significant byte is
`0x0F`. Zero denotes an absent or invalid handle. All other high-byte values
are reserved and rejected as handles. The invocation ABI still returns its
signed status separately from its unsigned result words.

Each Process handle table selects a fixed XOR mask over the low 56 bits when
constructed. The `0x0F` prefix guarantees a nonzero handle even when XOR clears
the entire payload. Only the handle module knows its internal slot/generation
encoding; kernel business code carries an opaque
`HandleValue`, and explicit ABI adapters serialize the masked value. Userspace
must not infer allocation order, payload bit fields, or numeric relationships.
The mask is not a security boundary and does not replace ownership or rights checks.
Slot reuse advances the generation, wrapping to one at its maximum without
retiring slot capacity. Numeric values can repeat: using a closed, replaced,
or moved handle is a caller error and may operate on a later entry with the
same value. Generation checks reduce accidental stale-value reuse; they do not
promise permanent invalidation. The Native table does not depend on a narrower
descriptor namespace.

Inspector enumeration and crash records expose exactly the masked value held by
the observed Process. Detailed handle queries decode in that Process's table,
not the inspector's. Cross-process transfers install a destination-local value;
KOIDs remain unchanged. A concurrently closed handle can disappear between
observation and lookup. The ABI adapter rejects values outside the `0x0F` class;
the table unmasks and validates the payload before using its slot index.

A handle entry contains a user-authority reference, rights, and handle flags.
Lookup under the process handle-table lock validates generation, object type,
supported rights, and required rights, then acquires a typed object reference.
The lock is released before object code, user copy, allocation, or blocking.
A concurrent close does not cancel an already resolved operation.

Handle slots and their Process accounting index use independently owned 4 KiB
pages. A page is reclaimable only when it contains no published handles,
unpublished reservations, or transfer claims. Close, reservation rollback/trim,
and committed moves detach empty page pairs under the Process locks and return
their physical backing outside those locks. Released bytes leave the cumulative
storage charge only after both pages have been returned. In-transit message
ownership does not retain a committed move's source page.

Sparse page-directory metadata remains allocated and charged until Process
retirement. It records the maximum generation used during each backing page's
lifetime; reconstruction advances that maximum with wrap and starts a new
maximum for the new backing. Empty page slots remain reusable after wrap.
Missing-page lookups fail without dereferencing released storage. Live handles
are never relocated, and partially occupied pages are retained. Reclamation
does bounded work over empty pages and their own free-list entries, without
periodic polling or a whole-table scan on close.

### Typed handle flags

The fixed-width `u32` flag field is an ABI representation, not one global flag
namespace. Flags are interpreted as the pair `(object kind, raw flags)`. Each
concrete object module owns a distinct typed flag vocabulary and declares which
raw values it accepts. Reusing a numeric bit for unrelated object kinds does
not give those flags common semantics. A type with no implemented flag
consumer exposes an empty flag type and accepts only zero; ABI space is not a
reason to publish speculative behavior.

Rights answer whether a handle authorizes an operation. Flags may refine how a
particular object type performs an already-authorized operation, but must not
silently duplicate a right such as `DUPLICATE`, `TRANSFER`, `WAIT`, `READ`, or
`WRITE`. Process inheritance is explicit through capability transfer, blocking
is an operation contract, and revocation belongs to typed lease lineages;
these policies are not generic handle flags.

The handle table stores raw validated bits so heterogeneous entries remain
compact. Typed resolution pairs the operation-pin reference with the concrete
object's decoded flag type before object policy can observe it. The SDK follows
the same rule: `OwnedHandle<T>` and its typed information associate `T` with a
flag type, while `OwnedHandle<AnyObject>` may report only raw bits until a kind
check and downcast establish their interpretation.

Move preserves the exact flag state. Duplicate and replace preserve flags by
default; a concrete type may define an explicit monotonic restriction, but it
may never use either operation to remove an existing safety restriction.
Startup publication and capability receive preserve the validated destination
state and never trust user-authored output flags.

The generic handle layer remains responsible for generations, reservations,
accounting, lock order, and atomic namespace commits. If a type-specific flag
affects a generic transaction, its module provides a side-effect-free policy
for the preparation and validation phase. Flag policy may not allocate, block,
acquire payload locks, inspect mutable hardware, or introduce a fallible step
after commit begins. This keeps object-specific interpretation from weakening
the all-or-nothing transfer contract.

Rights only decrease through duplicate, replace, or transfer. Generic rights
cover handle mechanics and genuinely common observation:

```text
DUPLICATE  TRANSFER  WAIT  INSPECT
```

Payload and mapping rights retain their literal meaning:

```text
READ  WRITE  MAP  EXECUTE  PROTECT  RESIZE  PIN
```

Task, VM, and hardware rights are named for the exact operation, for example
`START`, `REQUEST_STOP`, `RUN_VCPU`, `INJECT_INTERRUPT`, `GRANT_MEMORY`,
`ASSIGN_DEVICE`, `MAP_DMA`, `ACK_INTERRUPT`, and `REVOKE`. Object kinds declare
their supported mask. `WRITE` never means metadata mutation and never implies
`EXECUTE`. Mapping permission is the intersection of VMO rights, VMAR rights,
object policy, executable provenance, and requested protection.

Task construction and accounting policy use the equally literal
`CREATE_PROCESS`, `CREATE_THREAD`, `CREATE_TASK_GROUP`,
`CREATE_RESOURCE_DOMAIN`, `SET_LIMITS`, and `CREATE_EXECUTABLE` rights. A
factory or authority object grants only the named operation; every target
`TaskGroup`, `ResourceDomain`, VMO, and VMAR remains a separately resolved
capability.

`DERIVE` is specific to immutable view construction. Derivation requires the
source inspector's complete supported rights because the returned inspector
has a fixed rights set; callers must attenuate that handle before delegation.
The derived payload can only narrow scope and visibility. This avoids rights
amplification while allowing init to build a restricted view and then pass an
`INSPECT`-only handle to an untrusted observer.

`handle_get_info` reports handle-local kind, rights, and flags. A fixed
`object_get_basic_info` may report common identity under `INSPECT`. Lifecycle,
task, memory-accounting, and hardware observations use typed calls such as
`process_get_info`, `memory_inspector_read`, and `physical_device_info`; there
is no generic object-topic selector.
`file_get_info` and `directory_get_info` follow that typed model. They expose
live attributes and observation-only filesystem, mount, and node identities.
The extensible metadata calls also expose timestamp validity and normalized UTC
values. `directory_canonicalize` separately reports the spelling observed within
an explicit directory scope; that spelling is neither a reverse-lookup authority
nor a stable identity.

### Namespace management

`filesystem_mount_ramfs` and `filesystem_switch_root`
require `MANAGE_NAMESPACE | WRITE | EXECUTE` on an explicit namespace-root
Directory. Both its traversal root and current location must follow the live
namespace root; a fixed directory opened at `/` is not equivalent. The initial
process receives this right, and userspace decides whether to delegate it.
The default service manager transfers it to system init without granting it to
ordinary services or the recovery shell.

Mounting ramfs creates independent writable memory storage. Internal subtree
binding supports root promotion but is not a public syscall. Root switching
accepts another mounted
filesystem root, an attachment point for the old root, and at most sixteen
mount or RAM-subtree moves. A RAM-subtree move detaches its old directory entry
and creates a mount without copying file contents. Source paths resolve in the
old namespace; the old-root attachment and move destinations resolve under the
new root. Preparation does not change
the published root or move mounts. Root and mount edges commit together, and
a concurrent topology change returns `Busy` without a partial switch.

Explicit namespace-root anchors follow the new root. Fixed directory and file
capabilities keep their identities and confinement; a root-relative scope can
retain its old cwd while absolute lookup follows the new root. Promoting a RAM
subtree preserves existing File capabilities and scopes whose fixed root is
inside that subtree. A namespace-root scope whose cwd retains a location inside
the promoted subtree on its old mount becomes detached: relative lookup fails,
while absolute lookup still follows its namespace root. Paths such as
`/boot` and `/system`, service supervision and the choice to switch roots remain
userspace policy. See [VFS rooted scopes](vfs.md#rooted-directory-scopes) for
destination constraints and capability semantics.

### Object lifetime

The shared-reference count and active-handle count are different. Process handles
and in-transit capabilities are active; wait registrations and internal kernel
references are not. Transfer does not create a false zero transition. The last
active handle may detach state and enqueue an already-reserved teardown item,
but it may not recursively destroy contained capabilities, allocate, block,
log, or perform fallible hardware work. An iterative intrusive worklist drains
nested messages and capabilities without holding one object's lock while
decrementing another. Zero-handle resurrection is forbidden, and `Drop` never
accesses hardware.

`UserThread` is itself a kernel object. Kernel services, scheduler residence,
resolved operations, and Process handles retain classified references to the
same erased allocation and therefore the same KOID; the
scheduler payload remains a separate execution owner rather than a second
identity. The Thread object supports `WAIT`, `INSPECT`, `START`, and
`REQUEST_STOP`, and asserts its level-triggered `TERMINATED` signal after
scheduler detachment.

`Process`, `TaskGroup`, and `ResourceDomain` each admit exactly one
handle-visible object identity. Their publication claim is concurrency-safe,
rolls back if object construction fails, and becomes permanent once the
unpublished object exists; later code cannot manufacture a second KOID for the
same lifecycle. Kernel subsystems continue to hold classified direct
references rather than paying for private handle-table lookups.

Writable, immutable snapshot and executable VMO variants share one ABI kind but
publish different per-instance rights ceilings. A writable VMO cannot acquire
`EXECUTE`. Immutable snapshots grant `READ` and `MAP`; executable provenance is
internal to kernel loader preparation; ordinary userspace file mappings use
File EXECUTE authority plus content protection instead of an exported snapshot.
Neither immutable variant can acquire `WRITE`. Executable views may share the
same instruction-coherent immutable pages. A root VMAR
object retains the exact native address-space owner and its generation-checked
token, and is also published at most once for that address space.

Ordinary duplicated handles are not generically revocable. Revocable authority
uses typed lease lineages such as `MemoryGrant`, `DeviceLease`, `DmaMapping`,
and `InterruptSession`. Revocation prevents new descendants and completes only
after DMA stops, IOMMU invalidation, interrupt masking and drain, mapping
removal, and remote acknowledgement. Ambiguous teardown quarantines resources
rather than returning them to allocation.

### Supervisor-controlled process evidence

Ordinary processes retire automatically. An externally constructed child may
opt into bounded diagnostic retention through `process_builder_retain` before
seal/start. This requires `RETAIN_PROCESS`, inherited only from an explicitly
authorized TaskFactory. There is no running-process self-registration and no
kernel callback into a crash handler.

The supervisor owns one nonduplicable `ProcessRetirement` controller. It may
move that controller through a direct capability rendezvous. Process stop
closes admission and withdraws handles while already admitted transactions
cancel or finish safely. A peer observes channel closure independently of the
diagnostic retention interval. After every target thread has stopped, `READY`
publishes register copies, numeric handle records and selected resident private
pages. Handle metadata has no object references and cannot recreate authority.
The scene owns neither the address space, file protection nor device leases.
CPU, translation and DMA retirement still obey their existing safety rules.

`CrashView` is a separately delegable read-only view. It can enumerate thread,
mapping and handle records, and copy up to 64 KiB of retained memory per call.
It never faults the target, accesses MMIO, or consults a filesystem. Private
resident heap/stack and materialized COW pages can be retained; writable shared
backing and clean file pages are excluded. Pages whose backing would also pin a
larger contiguous allocation are excluded so retention cannot exceed its byte
budget indirectly. Missing ranges report `NOT_FOUND`,
and reads may return a contiguous short prefix. Truncation and excluded mapping
kinds are explicit. Register records use ELF `e_machine` and little-endian
64-bit words for architecture-specific register arrays.
For AArch64, general words are x0–x30 followed by a zero word, vector words
are low/high pairs for v0–v31, and floating-point control words are FPCR/FPSR.
For RISC-V, general words are x0–x31, the first 32 vector words hold f0–f31,
and the first floating-point control word is FCSR; unused words are zero.
These are userspace registers, never kernel stack or kernel register contents.

Explicit release, controller-owner loss, aborted construction or timeout revoke
all derived views; ordinary Process observers and duplicate CrashViews cannot
extend retention. New operations return `REVOKED`; already admitted bounded
copies may finish. `RELEASED` is sticky and can precede `READY`. The deadline
starts at the first stop request, including time spent quiescing. Supervisors
must release after inspection before joining final Process/TaskGroup retirement.

Current admission limits per registering supervisor are 64 armed children,
four memory captures and 64 MiB of page payload, with a maximum 60-second
timeout. Each scene contains at most 32 thread, 256 handle and 1024 mapping
records. Page-selection work is bounded, and metadata is separately charged to
resource domains. A byte limit is an upper bound, not a promise of a complete
dump. Quota/allocation pressure can yield partial or metadata-only evidence.
Retained pages remain charged to their original owner until released.

`make test-process-retirement` exercises this contract with real child
processes, private heap/stack contents, blocked channels, fault registers,
attenuated authority, deadline expiry, duplicated stale views and simultaneous
last-thread exits on separate CPUs. The fixture requires at least two vCPUs.

## Publication and output handles

Capability operations follow `prepare -> validate -> publish`.

Creation of at most two handles reserves table slots and values, constructs and
validates the complete objects and `NativeResult`, proves result encoding
infallible, and publishes the slots at the sole final commit. An error returns
no usable handle. If task termination wins after commit, ordinary Process
teardown closes the handles; the syscall never attempts a second rollback.

A variable-size receive cannot use result registers. It therefore:

1. claims but does not dequeue the immutable message;
2. acquires a quota-charged linear `UserWriteReservation` over the exact output
   ranges and reserves all receiver handle slots;
3. copies bytes and the future numeric handle values while those slots remain
   unresolvable;
4. enters a short, uncancellable final section; and
5. publishes every slot and consumes the message with no remaining fallible
   operation.

The reservation is bound to the address-space identity and ProcessImage
generation, prevents unmap, remap, and protection change until consumed, and
offers checked copy operations rather than Rust references. `ReceiveClaim`
restores the same message at the queue head on cancellation or copy failure. A
fault may leave meaningless bytes or unresolved handle values in the caller's
buffer, but it publishes no authority. Concurrent access to a syscall output
buffer is caller misuse: provisional values have no specified meaning until the
syscall completes, and may become usable during the final infallible commit of
an operation which will succeed.

## Channels and IPC

A Channel has two ordered endpoints. Each endpoint has one active handle or one
in-transit owner and does not support `DUPLICATE`. Threads in the owning process
share that handle normally. True multi-owner consumption belongs to a future
SharedQueue or dispatcher service, not weakened Channel ordering.

Each mutating operation holds a short endpoint ownership-epoch guard. A send
which transfers exclusive objects first acquires a `TransferClaim` for every
object in KOID order, prevents new mutations, and waits for existing guards to
retire. It then validates all claims, source uniqueness, rights, quota, and
queue capacity before changing any epoch or slot. Epoch updates, terminal
subscription events, source removal, and message publication form one
infallible commit. Failed preparation releases claims without changing epochs
or subscriptions; racing use of a reserved source returns `Busy`, not a false
stale-handle result. The endpoint used to send cannot be transferred in that
message.

Channel send copies user bytes and dispositions into owned memory before taking
object locks. It reserves message and accounting capacity, validates every
source generation, operation, kind, and rights attenuation, moves every source
entry into a private transaction, and publishes the complete message once. Any
pre-publication failure restores every entry to its original numeric handle.

An in-transit capability keeps the object's active-handle count but is not
usable. Send commit releases sender handle-slot charges and acquires one
in-transit charge from the sponsoring domain. Receive preparation reserves
receiver slots and charges; commit publishes them and releases in-transit
charges exactly once. Failure leaves the message and its charges unchanged.
Message destruction releases message, in-transit, and active-handle charges
exactly once. Persistent object memory remains charged to its original sponsor
unless a privileged charge-adoption transaction succeeds.

Channels have immutable, inspectable limits for queued messages, bytes, and
handles. Exhausted endpoint credits return `WouldBlock`; exhausted hierarchical
budget returns `QuotaExceeded`. Neither consumes input. `WRITABLE` means that a
minimum legal message fits; a larger write may still fail. A size-qualified
write reservation or capacity snapshot with an observation sequence prevents
retry livelock for large messages, and waiter ordering must prevent a stream of
small writes from starving an already queued legal large write. A message
retains its sponsoring resource-domain charge until consumption or destruction,
preventing quota laundering through transfer.

Queued endpoints can form active-handle cycles. ResourceDomain sponsorship is
therefore revocable across transfer. `resource_domain_revoke_all`, requiring
`REVOKE`, prevents new sponsored work, detaches endpoints and queues across
processes, and drives the iterative teardown worklist until cycles and leases
are quiescent. Ordinary handle owners do not gain sponsor-revocation authority.
Closing the final ResourceDomain control handle requests the same asynchronous
revocation; terminal observation is published only after quiescence.

The initial native surface has no `channel_call`. Request/reply libraries use
transaction IDs, Channel operations, and WaitSet. Any future kernel-assisted
call returns an explicit cancellable operation.

The initial Channel implementation permits Event handles in transfer
dispositions. A disposition containing a Channel endpoint returns
`NOT_SUPPORTED` without consuming any handle or enqueuing a message. Endpoint
transfer becomes available only with ResourceDomain revocation and iterative
teardown capable of reclaiming queued endpoint cycles; the disposition ABI
does not change when that restriction is removed.

### Physical-device DMA fault observation

`device_dma_fault_info` borrows a physical device with `INSPECT | ASSIGN_DEVICE`
and returns its first retained DMA containment report. An all-zero record means
no report, not an assurance of hardware isolation. The record and sticky
`DMA_FAULT` signal survive capability handoff. `WAIT` is granted when claiming
a device; inspection does not consume the record or clear the signal.

Reports identify the device's stream, source, scope, hardware code, original
monotonic timestamp and, when meaningful, DMA address and direction. Controller
failure reports expose no other device's address. The ABI does not select a
process, restart a device or stop a VM. Formal io-runtime consumes this mechanism
for journal reporting; bootstrap has no corresponding policy. See
[SMMUv3](smmuv3.md#formal-io-runtime-notification) for containment and retention.

## Waiting, signals, and atomic waits

Object kinds expose typed, positive, level state such as `READABLE`, `WRITABLE`,
`TERMINATED`, `PEER_CLOSED`, or `INTERRUPT_PENDING`. The raw mask is namespaced
and validated by object kind; SDK bindings expose `ChannelSignals`,
`ThreadSignals`, and other distinct types. Only Event and EventPair permit
arbitrary userspace signaling. POSIX signals, scheduler wakeups, and IRQ
acknowledgement are separate concepts.

Synchronous `object_wait_one` and `object_wait_many` resolve handles to object
references before registration. `wait_many` creates one wait transaction,
canonicalizes objects, snapshots and registers in KOID order or through a
sequence-validated equivalent, and revalidates every observation before
blocking. Its level-state linearization point and the single winner among
signal, timeout, and cancellation are explicit. Source-handle close does not
cancel the wait.

A readiness observation does not reserve data or destination capacity. A later
I/O operation may return `WOULD_BLOCK`; multiplexed event loops must use
nonblocking reads/writes and resume the complete wait set in that case. Waiting
again on only the selected object can strand other ready inputs. Backpressured
relays must retain bounded pending messages and wait for destination writability
while continuing to service their other directions.

Asynchronous waiting uses a WaitSet-owned, globally non-reused
`SubscriptionId`, not an arbitrary user key and not the source handle value.
Binding requires `WAIT` on the source object and a distinct `BIND_WAIT` right
on the WaitSet, retains the resolved object, and reserves exactly one
quota-charged event slot. A subscription follows
`Dormant -> Armed -> EventQueued -> Delivered -> Dormant`; publication fills or
coalesces its reserved slot without allocation or blocking, and rearm is legal
only after delivery is consumed. WaitSet size and event slots are bounded.
`WAIT` authorizes dequeue, not subscription mutation.

Subscriptions are object-lifetime operations, so source-handle close does not
cancel them. This implementation rejects WaitSet and CapabilityChannel sources.
CapabilityChannel support requires a future `OwnerEpoch` protocol that publishes
`OwnershipLost` on successful ownership transfer, preventing the previous owner
from retaining a readiness side channel. Failed transfer must change nothing.

Sets support 1 through 1024 subscriptions, are process-local and cannot be
transferred. `wait_set_wait` copies one 24-byte record containing registration
ID, observed signal bits and observation sequence. Copyout failure restores the
event unless explicit removal or set closure has cancelled it; rearm remains
busy until successful consumption. Final active-handle close detaches all
subscriptions and wakes blocked consumers. Source pins retain lifetime without
becoming active userspace authority.

Every relevant assert and deassert advances the object's observation sequence,
even when no subscription is armed. Bind and rearm atomically compare
`{typed_level_state, sequence}` with event publication. This lets Native
consumers detect ready/not-ready/ready transitions between one-shot deliveries
without making object waits edge-triggered.

A claimed PhysicalDevice exposes interrupt readiness through `READABLE`.
`device_irq_pending` returns its pending sequence, and
`device_irq_complete(device, sequence, asserted)` completes that exact request.
A stale sequence cannot acknowledge a later interrupt. This sequence belongs
to device IRQ completion, independently of the WaitSet observation sequence.

Each Thread has a `ThreadWaitContext` for wait-resource lifetime. It owns the
atomic condition slot and tracks generation-qualified signal subscriptions and
timeout registrations. Completing scheduler arbitration does not make the
context reusable: every signal node must be unlinked and every timer must be
cancelled or joined first. New wait admission, Thread termination, and retirement
reject remaining registrations. Multi-object waits share one scheduler ticket
but retain a separate registration for each signal source. Persistent WaitSet
subscriptions belong to the WaitSet, independently of a Thread blocking on it.
The scheduler's `WaitRecord` remains in CPU-owned scheduling state; the stable
resource context does not change scheduler lock ownership or migration rules.

Native atomic wait/wake currently use process-private aligned writable u32
words. The key contains the address-space, non-reused mapping token, and virtual
byte address. The kernel retains a writable backing lease through each admitted
wait and performs the value check and wait publication under the same sharded
condition lock used by wake. Scheduler tickets arbitrate wake, timeout and
cancellation exactly once. The condition registration lives in a slot embedded in the
scheduler Thread, together with its backing lease and per-wait resource charge.
Buckets link generation-qualified tickets and look up nodes under registry
protection; they retain no pointers into kernel stacks. A scoped registration
unlinks the node before releasing its lease and charge outside locks. Thread
termination and retirement reject a still-linked slot, and destruction checks
the same invariant. CPU migration preserves the Thread-owned slot. Thread/Process
stop resumes cancellation cleanup rather than discarding a parked continuation.
Finite deadlines still allocate and charge a timeout timer;
infinite deadlines do not require timer storage. The 64 hash buckets are not a
limit on waiter count.

A replaced mapping cannot wake an old wait; callers
must keep mappings live until their waits finish. Aliases and other Processes
have separate wait domains. Shared VMO keys, requeue and priority inheritance
remain separately specified extensions, not implemented contracts.

## Process, memory, and user-copy safety

Process owns its handle table, immutable current image, threads, address-space
capability, and membership in one TaskGroup and one ResourceDomain. TaskGroup
owns grouped lifecycle and stop. ResourceDomain owns hierarchical accounting.
Neither relationship grants privileged operations.

The bootstrap process receives explicit typed authorities such as TaskFactory,
VirtualMachineCreationAuthority, ExecutableAuthority, DeviceAuthority,
IrqAuthority, and DmaAuthority, plus a bootstrap Channel. Derived one-shot
leases bind individual VM construction transactions to a ResourceDomain. There
is no pseudo self handle or implicit root.

### Native 64-bit application address-space contract

The common Native contract separates application-controlled mappings from
system-managed user mappings. The machine profile determines the addressable
user range, and the architecture layout selects the application boundary within
it. Neither a universal 256 TiB envelope nor an equal split is an ABI promise.
All limits below are exclusive.

| Current profile | User address limit | Application address limit | System-managed user region |
| --- | --- | --- | --- |
| AArch64 VA48 | `0x0001_0000_0000_0000` (256 TiB) | `0x0000_8000_0000_0000` (128 TiB) | Application limit through user limit |
| RISC-V Sv39 | `0x0000_0040_0000_0000` (256 GiB) | `0x0000_0020_0000_0000` (128 GiB) | Application limit through user limit |
| Compact AArch64 VA42–VA47 | `2^VA_BITS` | Currently half the selected user range | Application limit through user limit |

These are provisional layout choices. A later profile may expand application
space or choose an unequal reservation without changing the common ownership
rule. Applications must use the VMAR range granted to their Process rather
than hardcoding these table values. There are no architecture-independent ABI
constants prescribing these numerical limits. x86-64 has no admitted Native
execution profile and gains none from this policy.

Applications and their allocators/loaders must not select system-reserved
addresses for images, libraries, heaps, stacks, or explicit VMAR reservations.
Each architecture supplies both limits through HAL `AddressSpacePlan`. Common
memory code consumes those limits without matching architecture identities or
computing a partition. Each address-space owner retains the immutable
application boundary at construction. The kernel enforces it at capability
publication: every handle-visible VMAR contains a checked application-only
token. A root overlapping the reservation cannot be published, and child
VMARs remain within their parent. The same authority bounds mapping, private
mapping, unmapping, protection and destruction. Internal kernel VMARs retain
the full machine-supported range.

System-managed mappings remain user virtual addresses, distinct from the
kernel's own privileged address range. They can accommodate a future vDSO or
shared ABI pages; individual permissions and discovery belong to the facility
which installs them. The reservation does not establish a fixed vDSO address,
grant application write authority, or allocate physical memory.

The kernel grants process ROOT_VMAR from one page up to the HAL application
limit, as specified in the [Native init contract](native-init.md#executable-image).
The main stack and both guards remain below that limit. System-reserved user
addresses remain outside application authority. RISC-V needs no wider translation mode to reserve system pages
within its existing Sv39 user range.

### Memory mappings and backing ownership

VMO and VMAR provide memory ownership and mapping policy. Executable mappings
require immutable executable provenance; writable authority cannot be upgraded
to executable authority. W^X is the default. A mapping operation consumes
typed user addresses and checked lengths, never Rust pointers.

`vmo_create_snapshot` captures coherent immutable memory-VMO bytes while
excluding writable mappings and exclusive hardware writers. Files have no
userspace snapshot-VMO operation.

`PROTECT` is independent File/Directory authority, attenuated during handle
copy, transfer and pathname derivation; it implies neither READ nor EXECUTE.
`file_protect` acquires content exclusion on the canonical file and returns a
process-local FileProtection token with MAP|INSPECT and the length sampled
under that exclusion. Existing writable handles, append and truncate cannot
bypass it. Names and other metadata remain mutable. The token cannot be
copied or transferred. Closing it releases only that reference; a mapping
keeps its independent reference until unmap or process teardown. Holding an
ordinary File elsewhere never extends a terminated process's protection.
After the owning process's threads stop and its handles close, teardown
detaches protected sources without allocating memory. A VMAR held elsewhere
may retain the dead address space's charged pages, but cannot keep content
protected or refill pages from the file. This release does not require a new
page-table transaction or destruction of every external VMAR handle.
Content writes and resize return `BUSY` while protection remains. Missing
`PROTECT` returns `ACCESS_DENIED`; acquiring protection does not itself read
or retain the file contents. Backend failure remains an error on subsequent
reads, including reads that could otherwise use cached pages.

`vmar_map_file` accepts File READ (additionally EXECUTE for executable views),
a matching FileProtection MAP token, and MAP on a VMAR owned by the calling
process. It uses the same source offset, data interval and zero-fill record as
`vmar_map_private`, but accepts only COPY_ON_WRITE mode. A token for a
different file or EAGER mode returns
`INVALID_ARGUMENT`. Mapping another process's VMAR returns `ACCESS_DENIED`.
It retains protected file identity and range metadata, not a whole-file
historical copy. Resident mapping pages and metadata are charged to the mapping
process. Clean pages
can be retired after page-table withdrawal and acknowledged TLB invalidation,
then faulted back from the protected file. Modified COW pages cannot be
discarded without another backing store. Kernel user-buffer copies must also
fault missing pages in before acquiring their operation pins.

Reclaim and refill are fallible. Explicit mapping or user-buffer copy syscalls
report failures such as `RESOURCE_LIMIT`, `NO_MEMORY` or backend I/O errors.
A hardware page fault has no syscall result: if bounded reclaim/refill retries
cannot resolve it, the kernel terminates the Process as an unhandled page
fault. Creating a lazy mapping therefore does not guarantee that every future
memory access will succeed. Reclaim is best effort; page-table withdrawal may
itself need temporary transaction quota and memory before it can release pages.

`vmar_map_private` installs an address-space-owned view of immutable bytes. Its
request names a page-aligned source offset, data offset within the first page,
data length, destination range, permissions and copy mode. Bytes outside the
selected data interval are zero, including ELF prefix padding and BSS. Complete
source pages may be shared; partial boundary pages are sanitized before mapping
publication. Private write permission derives from readable source bytes and
destination VMAR authority, and grants no ability to modify the source.

Copy-on-write mode initially maps shared pages read-only and materializes a
private page on first write. Eager mode prepares all private pages before
publication. Both modes provide the same isolation. Executable private views
additionally require an executable source VMO and source EXECUTE rights; their
sanitized pages are sealed and instruction-published before mapping, with no
writable alias. Writable private views never permit EXECUTE.
Ordinary shared writable VMOs and exclusive hardware backing retain
their stable-page lease contract. This is not a live address-space clone or a
snapshot of concurrently hardware-written guest memory.

Explicit `GuestMemory` READ and WRITE rights allow a userspace device backend
to copy up to 4096 guest bytes per call. They do not reopen direct VMO writes
or writable Native mappings. `guest_memory_scalar` provides naturally aligned
1/2/4/8-byte acquire loads and release stores; stores include a full memory
fence so used-ring publication precedes notification-suppression checks.
Concurrent Native payload operations return `BUSY` instead of mixing scalar
and byte accesses. Dedicating a grant to a `NativeBlock` permanently closes
these payload operations, including through retained duplicate handles.

`guest_interrupt_create` takes VirtualMachine WRITE and reserves one exclusive
AArch64 device interrupt line while the VM is installed or running. It returns
a `GuestInterrupt` with WRITE, TRANSFER and INSPECT rights. Built-in serial,
physical-device and notification lines cannot be reused. The owner controls a
level through `guest_interrupt_set_level`; the last active handle deasserts it.
Closing a source does not release its reservation before VM retirement, and
level updates fail once retirement starts. Device-register and queue semantics
remain in userspace.

A COW transition prepares replacement page ownership and an immutable machine
root, then commits through the existing address-space epoch and acknowledged
cross-CPU replacement protocol. Old mapping versions retain concrete backing
until translation retirement. Kernel copyout, transactional user-write
reservations and atomic-word pins materialize private pages before accessing
them. A source page is never made writable merely because its reference count
appears to be one. Shared source allocation is charged once; private pages and
view metadata are charged to the destination address space.

Partial unmap removes unused private pages from the next mapping version;
retired versions retain them only until translation acknowledgment. Atomic-word
pins retain their concrete private page rather than the whole private view.
COW preserves virtual mapping identity and every already-writable page, so it
can proceed while an unrelated transactional output reservation is parked.
Only overlapping unmap and protection changes are excluded by those reservations;
unrelated thread-stack and heap reclamation can proceed. Long-lived private
output pins retain only their covered pages. VMAR
services retry unpublished stale-epoch transactions; this does not hide genuine
reservation conflicts behind an unbounded wait.

The existing `UserAddressSpace: ForeignMemory` direction remains mandatory.
Syscall code represents user memory as fixed-width `UserAddress` and
`UserSlice` values. It performs checked arithmetic, bounds lengths before
allocation, copies variable input into owned kernel memory, and validates in
the schema-defined observable order. No Rust reference to user memory survives
a mapping change, lock release, block, or context switch. Ordinary kernel locks
are not held while user access can fault.

ResourceDomain quotas cover at least processes, threads, handles, objects,
kernel bytes, committed and pinned pages, guest pages, IPC messages/bytes/
handles, subscriptions, timers, VMs, vCPUs, device leases, and DMA mappings.
One `ChargeReservation` validates and reserves every affected dimension over
the complete root-to-leaf quota path under a fixed transaction/lock order.
Pending reservations count as usage, partial failure rolls back, a limit cannot
fall below current usage plus reservations, and child limits remain bounded by
ancestors. Process, TaskGroup, and ResourceDomain metadata is charged to the
parent domain. The kernel retains a separate emergency budget which untrusted
domains cannot consume.

## Implemented Native object and syscall surface

The compiler-checked schema owns object-local selectors, signatures and completion
contracts. These are representative implemented families; consult the
[generated reference](https://github.com/HypeR-Proj/abi/blob/main/docs/native.md) for the complete surface.

| Family | Objects and representative operations |
| --- | --- |
| ABI and observation | SystemInfo queries and clocks; scoped inspectors |
| Handles | Close, duplicate, replace/attenuate, handle and object information |
| Accounting | ResourceDomain and TaskGroup creation, resource sponsorship |
| Tasks | ExecutionContext for current execution; ProcessBuilder construction/seal/start/abort; Process stop and exit; Thread create/start/exit/yield/sleep/request-stop |
| Memory | VMO creation/read/write/snapshot; protected file mappings; VMAR allocate/map/private-map/unmap/protect/destroy |
| IPC | ObjectFactory creation; ByteChannel queues, CapabilityChannel rendezvous, Event create/signal |
| Wait | Object wait-one/wait-many; WaitSet create/add/rearm/remove/wait; atomic wait/wake |
| Filesystem | Capability-relative Directory operations, File data/metadata/sync, content protection and advisory locks; authorized ramfs mount and atomic root switching |
| Virtualization | Creation leases, PendingVirtualMachine construction/install/abort, VM stop and power completion, vCPU start/affinity/MMIO completion, VirtualSerial |
| Physical devices and I/O | DeviceAssignmentAuthority, PhysicalDevice claims/MMIO/IRQ, GuestMemory, GuestMapping, GuestMailbox, GuestNotification, GuestInterrupt and NativeBlock |

Device claims require explicit assignment authority and validate the selected
resource profile. Userspace device drivers use bounded `device_mmio` operations
and IRQ pending/completion calls; the current ABI does not expose a generic
DeviceLease-to-physical-VMO mapping interface. Contiguous VMOs and
`vmo_get_dma_extent` provide the admitted DMA backing path. Device assignment
and guest mappings retain their backing through acknowledged retirement;
quarantined devices may retain it until host reboot.

Timer, EventPair, Counter, exception endpoint/token, atomic requeue, and generic
DMA-map/revoke calls are not current Native operations. `ExecutableAuthority`
and its creation right remain reserved; file EXECUTE authorizes executable
protected mappings, not an exported file snapshot VMO. Reserved rights or accounting fields do not establish callable APIs.

## AArch64 Tier-1 execution

AArch64 requires FEAT_VHE. Native EL0 uses the host EL2&0 translation regime with `E2H=1` and
`TGE=1`. The stable permanent kernel hierarchy occupies the configured
canonical upper range through `TTBR1_EL2`; per-Process roots contain only
private user mappings in the configured lower range through `TTBR0_EL2`.
Guarded-stack and other runtime kernel mapping mutations therefore update only
the kernel hierarchy and never require replication into Process roots. A user
root cannot retire while its context is active. Returning to a vCPU clears the
host-user regime explicitly. Exception code must never infer the return regime
merely from a lower-EL vector.

Threads in one Process may execute concurrently on several CPUs. Native roots
therefore carry an active-CPU residency set, mapping epoch, acknowledged
shootdown, and stable software identity. Runtime ASID leases belong to a
separate rollover namespace from guest VMID leases. Mapping removal cannot release physical backing until every relevant
CPU has acknowledged the old root's retirement. AArch64 disconnects the old
lower hierarchy through the permanently retained empty boot root before
invalidating its ASID; context synchronization prevents speculative old-root
refills after invalidation. Replacement installs the new root only after that
invalidation completes. Hardware qualification must
verify exception routing, SMP migration, cache and TLB maintenance, FP/SIMD,
TLS, counter access, and transitions between Native EL0, the host, and guests.

RISC-V implements these Native execution contracts through Sv39 U-mode roots,
qualified trap returns and acknowledged translation retirement. Its RV64GC
LP64D applications use `tp` for runtime TLS and preserve full integer/FP state.
x86-64 ring-3 entry remains unsupported. The shared contracts retain
AArch64's world-regime and translation distinctions.

## Implementation and acceptance

Native applications and userspace-managed VMs run on AArch64 and RISC-V.
The implemented surface includes capability inspection/transfer, Process and
Thread lifecycle, IPC and WaitSets, VMO/VMAR mappings and COW, filesystem
operations, clocks, atomic waits, and VM/device management. The generated
[schema reference](https://github.com/HypeR-Proj/abi/blob/main/docs/native.md) is authoritative for syscall
numbers and argument contracts. The [Native init contract](native-init.md)
describes executable loading and service bootstrap; the
[current status](https://github.com/HypeR-Proj/HypeR/blob/main/docs/project/status.md) records support and remaining limits.

Verification follows the affected boundary:

- Host tests exercise portable ownership, lifecycle, handle and ABI mechanisms.
- Kernel self-tests exercise direct calls, COW copyout, blocking continuation
  migration with FP/TLS preservation, user faults, Thread/Process stop, ASID
  rollover, and resource retirement.
- Native QEMU acceptance exercises the assembled SDK, std and apps through
  userspace init, services and the command shell.
- Guest and I/O VM acceptance exercises userspace VM management, console and
  backing-disk operations; runtime-crash tests cover retirement and restart.

The near-term integration target remains a trusted Linux I/O VM on Pi 5,
consumed through the [I/O VM package contract](https://github.com/HypeR-Proj/HypeR/blob/main/docs/design/virtualization/io-vm.md). Native
services provide management and explicit memory/device authority; standard
virtio-scsi queues and Linux vhost-scsi/LIO carry storage requests. Linux-local
ioctls do not become Native syscalls. Physical cache/TLB, interrupts and DMA
retirement still require hardware qualification; QEMU cannot establish those
properties. Future work is tracked in the [roadmap](https://github.com/HypeR-Proj/HypeR/blob/main/docs/project/roadmap.md), not
as additional mandatory phases of the Native ABI.

## Open implementation questions

The following extensions remain outside the implemented ABI until their
contracts and validation are reviewed:

- the exact x86-64 secondary result registers;
- whether the initial surface includes a dedicated SharedQueue or BackendSession
  for large asynchronous workloads;
- the complete priority-inheritance atomic-wait contract.

These questions remain outside the current ABI design until resolved.

### Native thread creation affinity

`thread_create(request, request_size)` copies a size-delimited
`thread_create_request` containing entry, stack, TLS, argument and affinity
pointer/count, plus reserved-zero flags and a reserved word. It creates
an inert Thread; `thread_start` publishes runnable execution. Passing zero for
both affinity arguments inherits the calling Thread's current allowed CPU set,
including changes made after that caller was created. An explicit affinity is
one to four little-endian `u64` bitmap words. Empty masks, unsupported CPU bits,
unmapped payloads and masks with no schedulable CPU fail before publishing a
child handle. As with Process affinity, offline bits may remain in the allowed
set if at least one selected CPU is schedulable. Affinity constrains both initial
placement and automatic Fair balancing; it does not reserve CPU capacity or
promise equal time across the selected CPUs.

### Public system configuration

`system_config(key)` returns a scalar in `value0` and zero in `value1`. The first
key, `SYSTEM_CONFIG_PAGE_SIZE`, reports the Native mapping granule in bytes from
the kernel memory implementation. It is immutable during a process lifetime.
Unused argument registers must be zero (`INVALID_ARGUMENT` otherwise); unknown
keys return `NOT_SUPPORTED`. No inspector authority is needed for this public
configuration. New public scalar keys can extend the same syscall.

Runtime stack/heap alignment derives from this query. It does not relax the
kernel VMAR service's independent page-alignment, nonempty-range, checked-address
and permission validation. Thread SP alignment remains the architecture calling
convention's alignment, not page alignment.

### VMAR address selection

`vmar_allocate(parent, address, size, options)` always interprets address as
the low end of the requested interval. With options zero, nonzero address is
a hint: first try it, otherwise choose the nearest fitting free interval
(lowest base wins equal distances). Address zero with options zero chooses the
lowest free range. `VMAR_ALLOCATE_EXACT` requires that exact address, including
zero; no relocation occurs. Unknown flags are INVALID_ARGUMENT. Addresses and
nonzero sizes must be page aligned and representable without overflow.
Success returns child in value0 and actual base in value1. The parent's authority
and system-reserved boundary constrain every mode. Selection and reservation
commit atomically against concurrent mappings/child reservations. No fitting
free range is NO_MEMORY; exact overlap keeps the existing conflict status.

The implementation stores VMAR identities in a persistent AVL tree and keeps
an augmented free-interval tree for each VMAR. Subtree maximum free lengths
allow nearest-hint selection without scanning all reservations. Creating or
destroying a VMAR takes O(log V + log F) index work and path-copy allocations,
where V is the address space's VMAR count and F is the affected free-interval
count. Unchanged subtrees remain shared with concurrent snapshots. Every node
allocation is fallible and accounted; failed preparation leaves the published
index unchanged. Actual mapping records also use a persistent AVL tree, keyed
by virtual base address. Point lookup takes O(log M); a range scan takes
O(log M + K), where M is the mapping count and K is the number of intersecting
records. Inserting or replacing one record copies only its tree path. Unmap and
protect replace the affected fragments, preserving unrelated subtrees.
Mapping transactions publish their free-space changes together with the mapping
snapshot; displaced backing remains owned until machine retirement completes.
Private-view version changes still reconcile all aliases, and machine page-table
preparation may visit the complete mapping set. These costs are separate from
the address index, so whole map/unmap/protect operations are not promised to be
O(log M). Tree nodes have more per-entry overhead than a packed vector.

SDK main and worker stacks use independent automatically placed child VMARs,
including their guard pages; they do not consume a fixed SDK stack arena. The
kernel supplies a temporary bootstrap stack. The runtime creates the final
main stack through its ordinary stack allocator, copies startup data, switches
SP, then retires the bootstrap stack. Static programs also retire their selected
loader image; dynamically linked programs retain the formal loader for its
continuation, constructors and later dynamic-link operations.

`SYSTEM_CONFIG_APPLICATION_ADDRESS_LIMIT` (key 2) returns the selected HAL
application-exclusive address limit in value0, with value1 zero. It describes
the architecture profile, not authority over a particular VMAR. The
kernel-provided ROOT_VMAR covers one page up to this limit; libraries must still
obey their handles.
