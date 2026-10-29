<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Virtual filesystem architecture

HypeR keeps filesystem namespace, path-resolution authority, open filesystem
objects, and the shared data/metadata cache in the kernel. A filesystem implementation is
an adapter below that policy boundary: it may execute directly in the kernel
or proxy an external service without changing the Native capability model.
Block devices form a separate backend boundary. A filesystem is not required
to be block-backed, and a block driver is not part of the VFS object model.

The initial root is a writable, kernel-resident ramfs initialized from the
immutable boot archive. Userspace can replace it through the generic namespace
operations described below. Namespace traversal, ramfs metadata, storage and
synchronization remain in the kernel. Other filesystem implementations and
block services may run in userspace; neither placement is required by VFS.

## Layer boundaries

`src/fs/primitives` contains reusable, policy-free filesystem mechanisms: validated names
and paths, stable backend node identities, metadata contracts, and the
hierarchical RAM filesystem importer. It has no Process, handle, scheduler, or
global-namespace dependency.

`src/fs/vfs` owns kernel policy: filesystem and mount identity, the system
namespace, capability-relative resolution, Directory and File kernel objects,
resource accounting, and the process-facing service boundary. Syscall parsing
and user-memory validation stay in the Native ABI entry layer or its service
adapter; backend code never observes a Process.

`src/fs/cache` owns reclaimable file-data and namespace-metadata residency. It does not resolve
paths or grant authority, and a cache miss never calls back into VFS policy.
CPU cache maintenance remains architecture/HAL work and is unrelated to the
filesystem data cache.

## Capability model

Native applications do not receive an ambient root or current working
directory. A Directory handle is explicit authority to resolve beneath one
location in one namespace view. Absolute-looking paths restart at that
capability's traversal root, not at a hidden system root; parent traversal is
clamped at the same boundary.

A File handle names one opened node and carries only the rights selected when
it was created. Opening may attenuate authority but never amplify it. `READ`
is always required on the source Directory for traversal, and every requested
File right must also be present on that Directory before the result is bounded
by the node's capability ceiling and mount policy. In the credential-free
root filesystem, a new read, write, or executable grant requires at least one
corresponding mode bit (`0444`, `0222`, or `0111`). No user/group identity or
Unix permission-selection policy is implied. Existing open capabilities retain
their rights after mode changes. Creation may grant the creator its requested
rights even when the initial mode is zero. `SET_ATTRIBUTES`, `LOCK_FILE`, and
`PROTECT` are separate rights from data access. File or child-directory
`PROTECT` authority must already be present on the source Directory.

Native reads and writes use explicit offsets; append selects the current end under
the same per-file lock that commits the write. SDK file objects may maintain their own offsets. Unix file-descriptor tables,
credentials and POSIX path semantics are not implied by this Native API.

The namespace strongly owns mounted filesystems. Directory and File objects
hold the namespace view and the resolved mount/node location needed to keep an
open object valid after namespace detachment. Mounts do not strongly reference
their namespace. File-data cache pins own only a content record, which has no
mount, namespace, backend or active-node backreference. Namespace discovery
references to that record and its active node are weak, so content retention
cannot form a namespace--mount--node cycle or acquire file authority.

## Backend contract

A backend receives validated names and opaque, counted node leases. Its results are
bounded owned metadata or caller-provided buffers; it may not retain borrowed
VFS storage. Namespace, mount, handle-table and cache-index spinlocks are
released before backend operations. The shared sleeping content gate spans
regular-file I/O. A later remote adapter may block or perform IPC under that
gate without retaining a spinlock.

User loaders receive an ordinary File with explicit `READ | EXECUTE | PROTECT`
authority. They protect the content before reading the length or ELF headers,
then map selected file ranges into their own address space. No whole-file
snapshot VMO is exposed. Protected mappings retain file identity and read
through the ordinary cache on demand; they do not retain unrelated ELF data.
Backing failure remains an error even when cached bytes are available.

The kernel's restricted bootstrap-loader parser has a separate temporary input:
it borrows immutable boot archive bytes or copies the selected loader under the
content gate. Owned parser bytes are charged to the requesting child domain,
never to a shared filesystem sponsor. Segment installation copies the required
bytes into the new process, after which the temporary parser input is released.

The ramfs backend is synchronous and kernel-resident. Import does not require
a userspace server; opening and loading `/init` runs on a scheduled kernel
worker because runtime file operations use sleeping mutexes. Import builds
explicit root, parent, name, and node-identity relations.
Missing ancestor directories are synthesized deterministically, while an
archive that uses a non-directory as an ancestor is rejected before mount
publication.

## File-data cache

The common VFS layer owns file-content coherence. Ramfs nodes embed one
`FileContent`; cacheable remote nodes share a `FileRecord` which owns it. Independent
opens and aliases therefore use the same content gate and identity. Its
sleeping gate protects a non-repeating content revision and optional known
length. Cache entries are keyed by filesystem identity, node incarnation,
content revision and page index. File-data keys never use a path or raw reusable inode number. A clean page also retains an opaque content-owner pin; the portable
cache has no dependency on VFS types. This preserves remote file identity across the
last close and a later open while pages remain, including opens by another
process. Page reclamation releases these pins outside the cache lock. The record
contains only its identity, content state and quota charge; advisory locks and
mount pins remain on the active node. Cached records do not keep unlink busy.
After every record owner disappears, reopening requires a fresh incarnation.
Remote bindings use individually charged storage, released when the binding is
removed. If retained content exhausts a mount sponsor or ancestor quota,
unpublished preparation can request a finite domain-scoped cache sweep and
binding cleanup. Cache admission remains paused through that retry; neither
backend mutations nor handle-publication callbacks are replayed.

Reads, writes, append, resize, open-with-truncate, content protection
acquisition and temporary kernel loader reads use this common gate. A mutation advances the revision and forgets the known length
before calling the backend, including attempts that later return an error:
part of the file may already have changed. Old cache entries remain unreachable
and are reclaimed by ordinary eviction. Open-with-truncate retains its existing
prepare-handle, change-contents, publish-handle transaction. This implementation
serializes operations on the same file for one backend batch; a multi-batch
Native read is still not an atomic file snapshot.

The clean-page residency state machine remains:

```text
Vacant -> Loading(generation) -> Clean(valid length)
                  | failure
                  v
                Vacant
```

Only one caller owns a loading reservation. Backend fill occurs outside the
cache lock, and completion must present the same generation token before it can
publish. A failed or stale completion cannot overwrite a recycled entry.
Published pages are immutable shared owners; eviction removes the index owner
but cannot invalidate an active reader. Payload reservations remain charged
until the last owner releases the page, covering loading, resident and evicted
but still referenced pages. Each production payload owns one physical 4 KiB
page, including partial EOF tails; it is accounted separately from allocator
metadata. Hash buckets index loading and clean entries, a free-slot stack finds
vacant slots, and an indexed minimum heap tracks the oldest clean entry. Hits
refresh the access sequence; fill completion preserves the reservation sequence.
Victim lookup is constant time and heap updates are logarithmic in capacity.
Hash chains contain at most 64 entries, bounding index work while interrupts
are masked. A colliding miss bypasses retention once that limit is reached.
Shrinking metadata may discard entries that would exceed a merged bucket's
limit; growth preserves resident entries. Mapped canonical keys are migrated
first and never discarded: their count and bucket occupancy bound the smallest
replacement. A live mapping does not otherwise prevent metadata compaction.

The same cache also owns immutable directory batches and exact-name attribute
records for remote filesystems. Key kinds distinguish file data, directory
positions and namespace lookups. Full names and private checkpoints validate
hashed placement. Each metadata payload still occupies exactly one physical
page and retains only a plain content/accounting record, never a namespace
Location or backend. Directory streams retain their own small continuation;
cache eviction does not force a prefix rescan or pin a whole directory. The
same page permits, index accounting, pressure admission and reclaim paths apply
to all three kinds. Remote identity bindings use individually charged balanced
indices by ID and canonical path, with bounded successor-based cleanup rather
than a full registry scan on each open.

Cache capacity grows on demand rather than imposing a fixed byte budget. The
`kio-reclaim` kernel thread grows metadata geometrically and reclaims cold clean
pages in bounded batches. It temporarily stops new loading reservations, waits
for existing loaders, and parks the table with a constant-time ownership move.
Allocation, migration and destruction happen outside the cache lock. Reads
bypass retention while the table is parked. Failed growth restores the old
table; pressure can release empty oversized metadata before attempting a small
replacement, leaving a disabled cache if even that allocation fails. Sparse
metadata is also compacted after reclaim lifts memory pressure. Smaller
replacements may use the admission reserve to release a larger old table;
allocation remains fallible. Index arrays and shared page descriptors have
dedicated slab/large-allocation ownership, counted as file-cache backing and
released without retaining empty CPU-magazine objects.

Admission uses allocator-managed RAM, excluding firmware and other unmanaged
regions. Normally the high watermark stops admission at 90% utilization and
reclamation continues to 85%. Small heaps also retain a minimum free reserve.
Checks for each physical payload and conservative metadata reservation occur
under the allocator lock, so concurrent fills cannot overbook cache headroom.
Live non-cache allocations may still consume that headroom. Loading and
reader-pinned pages remain charged until their final release, and load
completion, memory release or retirement of the last mapping pin wakes a
pressure-blocked worker, even if the cache still owns the payload. Cache admission
never waits for reclamation: a miss falls back to ordinary backend I/O.
The background mapping sweep remains finite across those wakeups until free
memory reaches the resume watermark. Refaults do not restart the sweep and
continually evict a running application's code; an explicit allocation request
can still initiate its own bounded pass.

Ordinary allocation failure also permits a bounded, nonblocking attempt to
reclaim at most 64 unpinned clean pages before retrying once. This runs outside
allocator and cache locks and uses only the audited page/content-record
payload, whose destructor performs no filesystem or scheduler operation.
The generic allocator never sleeps to reclaim memory. Audited, unpublished
allocation preparation can additionally request a scheduled reclaim pass for
the actual buddy order or denying resource domain, even when usage is below
the pressure watermark. The worker scans a finite snapshot in bounded batches
and does not wait for pinned pages, in-flight loads or filesystem locks.
It then visits protected file mappings process by process and in increasing
virtual-address order, examining at most 64 pages per batch. Each range is
advanced before its retirement attempt, so refaulting pages or a conflicting
mapping operation cannot keep the sweep on the same address. Newer processes
and pages refaulted behind the cursor wait for a later pressure request.
Requests use non-repeating generations; cancellation and delayed completion
cannot complete a later request. Request serialization ends before retrying
preparation, while a separate guard keeps cache admissions paused through the
retry. Mutating syscalls are not replayed. Competing ordinary allocations or
unreclaimable fragmentation can still cause a bounded retry to fail.

Mapped-page reclamation is best effort: preparing a replacement mapping
transaction and page-table root needs temporary memory and requester-domain
quota before the old pages can be safely released. A hard quota with no such
headroom can therefore prevent reclamation and leave the original operation
failing with insufficient resources. Reclaim never borrows the filesystem
sponsor's or a global domain's quota to bypass that limit. Process teardown
detaches file-source protection without this fallible replacement step.

The memory observer includes cache payloads in used kernel memory and reports
unpinned indexed pages as reclaimable; pinned and loading pages are not
reclaimable.

An evicted payload can be reused only after acquiring unique ownership, with
no remaining reader or weak reference. Its allocation permit follows that
owner throughout the refill. The exact-page allocation can hold either a full
page or an EOF tail without growing. Readers holding an evicted page keep
immutable bytes. Refill, allocation and destruction occur outside the cache
lock; pressure prevents recycled pages from admitting new cache contents too.

The backend-neutral reader preserves consecutive miss ranges up to the native
512 KiB batch. Page-aligned remote reads reserve exclusive private fill pages
and let the backend fill consecutive misses directly through scatter slices.
These pages become immutable before publication; immutable hits and completed
fills remain pinned until the later userspace copy finishes. The remote adapter
validates worker-supplied physical extent plans and copies from the dedicated
I/O VM exchange pool directly into these destinations. No cache page is mapped
writable into the worker or made device-visible. Unaligned requests and cache
admission pressure retain the bounded scratch fallback. It issues no read
outside the requested range. Only fully returned pages, or a page-start-to-EOF
tail validated against the known length, are eligible for retention. Partial
cold reads need not populate the cache; partial warm reads can use complete
cached pages. Short backend reads retain their ordinary short-prefix semantics,
and backend errors are never hidden as cache misses.

Backends provide raw range I/O, metadata, stable node identity and a cheap read
admission check. Cached data and cached length still honor a failed or closed
backend once that state is latched; the check does not probe the media on a
cache hit. Remote filesystems use this common reader; it owns no file-page cache algorithm.
Writes continue through the existing synchronous backend path and explicit
`file_sync` durability contract. Dirty-page writeback, external coherence,
direct I/O and shared writable mappings require separate protocols. Destructors
perform no fallible storage work.

## Concurrency and lifecycle

Namespace resolution and file-data backend operations run in scheduled kernel
context. Short, nonblocking active-handle retirement callbacks may also execute
from masked Native handle-close entry.
Namespace traversal retains a node lease before entering file-content policy.
The file operation lock order is common content gate, then backend volume or
node-data lock. Cache spinlocks cover only short residency transitions and never
span backend execution, userspace copy, allocation, destruction or scheduler
blocking. The sleeping content gate may span backend I/O; it is released before
preparing or copying a userspace destination. Backend callbacks must not reenter
the common file operation and acquire the same content gate recursively.

Mount construction is prepare-then-publish. Publication is the only point at
which a namespace can discover the mount. Unmount first freezes new admission;
open files, directory capabilities, active lookups and child mounts make it
return `Busy`. A remote mount then drains backend operations and synchronizes
the volume before atomically removing namespace reachability. Failed sync or a
concurrent topology change leaves the mount attached and reopens admission.
Successful publication closes the remote filesystem's health state; stale cache
records cannot bypass retirement. The userspace filesystem manager stops the
worker and returns the block capability. There is no forced or lazy unmount.
Final destruction releases only already-quiescent memory; waiting and fallible
storage work belong to the explicit retirement path.

## Directory streams

`directory_begin_read` derives an independent DirectoryStream from Directory
READ authority. Its grant inherits only the source rights supported by the
stream: READ, DUPLICATE, TRANSFER and INSPECT. Derivation cannot restore
attenuated propagation or inspection rights. The stream pins the opened
namespace location and retains only its backend position and the previous
position needed to retry failed copyout.
It never pins complete directory contents or cache pages. Closing the stream
releases its namespace reference; duplicate and transferred handles share one
scan. Separate calls to begin_read have independent progress.

`directory_read` consumes a stream-issued continuation cookie and returns up to
four owned entry records. Cookie zero begins that stream, and a returned zero
means EOF. A short nonempty page may still carry a continuation. Cookies are
monotonic, globally non-reused and bound to one stream; only the most recent
input may be retried, including an EOF page. Forged, foreign-stream and older
cookies fail. Copyout failure leaves the previous position available for retry
without retaining cache bytes. Remote mutations of the scanned directory, or
renames of that directory or an ancestor, invalidate its scan. Mutations of
unrelated directories and content/attribute changes preserve traversal.
Applications can start a fresh stream after namespace changes. No stream provides
a directory snapshot.

The Rust Native SDK owns a stream per `DirectoryReader`; Rust std uses the same
contract for `ReadDir`. `DirEntry::metadata()` remains a fresh, non-following
lookup relative to its directory, allowing the kernel to reuse current cached
attributes without returning permanently saved application-side metadata.

Directory enumeration resumes from bounded checkpoints even when retention is
unavailable, so its backend scan remains linear in directory size. Canonical
name and attribute reuse depends on cache admission: under pressure, a fresh
per-entry metadata lookup can still rescan a FAT directory. Therefore tools such
as `ls` can incur repeated lookup work even though enumeration itself no longer
restarts at the beginning. Metadata invalidation is currently volume-wide;
unrelated writes can discard attribute and directory-batch reuse, but do not
invalidate a stream's physical continuation.

## Writable ramfs

Each open node lease pins the node independently of its directory entry.
Unlink removes namespace reachability; existing File handles continue to read
and write that same node. Recreating the name produces a distinct, non-reused
node identity. Removed directories reject creation through old handles.
Ramfs stream positions use monotonically allocated backend cookies, so deletion
cannot turn a position into a reused entry index.
Enumeration is not a snapshot of the whole directory during concurrent mutation.

File bytes borrow the boot archive until mutation requires owned storage.
Growth is zero-filled and charged to a filesystem-owned 256 MiB storage domain,
including node and directory-entry allocations. Replacement buffers reserve
before allocation and retain the old charge until replacement succeeds.
Truncation to zero releases owned capacity. The namespace mutation mutex
serializes create, remove, link, and rename publication; independent file reads and writes use
per-node sleeping mutexes. User memory is copied through bounded buffers before
or after backend execution. One Native write may complete a short prefix;
append placement and that prefix's write are atomic together.

File creation prepares the node, accounting and handle publication before
committing its name. Failed preparation leaves the namespace unchanged.
Temporary kernel loader inputs borrow unchanged archive bytes or own a
requester-charged copy taken under the file lock. Userland file mappings instead
retain explicit content protection for as long as they depend on the file.

Ramfs bypasses page retention because its owned bytes are authoritative storage,
while sharing the common file-content coordination contract. Cached writable
backends use the same revision and mutation protocol; a clean read cache does
not require dirty-page writeback. The backend seam
in `instance.rs` separates node leases, owned metadata, data operations and
temporary loader reads from path policy. The generic userspace adapter owns canonical path identities and ordered
request lifetime/cancellation there. Workers own media parsing and write layout;
the kernel executes validated read plans through the mount-bound block queue. [Filesystem services](https://github.com/HypeR-Proj/HypeR/blob/main/docs/design/storage/filesystem-services.md)
describes the bounded shared-buffer protocol, failure retirement and sequential
readahead. FAT parsing runs in `/svc/fs-fat`, selected on demand by the
`/svc/fs-backend` manager. The trusted Linux I/O VM remains the physical block
backend; see the [I/O VM contract](https://github.com/HypeR-Proj/HypeR/blob/main/docs/design/virtualization/io-vm.md).

The Native surface includes atomic open/create/create-new/truncate, file
write/append/resize, directory creation, rename with replacement, regular-file
hard links, symbolic links, metadata and timestamp updates, and removal of
files or empty directories. Open/create prepares handle publication before any
namespace or truncation commit. Failed publication cannot truncate an existing
file. Hard links share the same canonical node, contents, metadata, and lock
domain. Directory hard links and cross-filesystem rename/link are rejected.
Ramfs `file_sync` completes its in-memory operation; it does not promise durable
storage. Shared writable mappings and persistent writeback remain absent.

## Native file transfer batching

File transfer requests accept up to 2 MiB; this limit is independent of the
64 KiB VMO transfer limit. Reads process the request through the existing
File/backend interface in at most 512 KiB batches. Eligible page-aligned remote
reads retain immutable cache-page owners and copy those pages directly to the
caller, including newly filled cold pages. RAM files, unaligned ranges and cache
admission pressure use fallible scratch storage charged to the caller; small
fallback reads up to 1 KiB retain a stack buffer. Only the current batch's user
destination is prepared and pinned, after the backend has released its locks. Syscalls run in scheduled,
interruptible kernel context.

Reads stop at the observed EOF, a short backend read, or an error. An error
before any copy is reported to the caller; after completed batches, the copied
prefix is returned. Callers must still handle short reads. The operation is
not an atomic snapshot across concurrent file mutations. Scratch storage is
released on return and is not a file-data cache. Writes accept the same request
limit and accept at most 512 KiB per call, returning a short count for larger
requests. The kernel resolves and pins the accepted user input before taking
filesystem locks. Remote writes copy it directly into the mount's kernel-owned
read-only source window while holding the transaction lock; no writable user
alias exists to that window. The worker can submit source ranges only under the
active operation token. RAM files and other copied paths retain bounded scratch
storage. Input preparation is not a snapshot against concurrent application
writes: the stable accepted bytes are those copied into kernel storage. Append
offset selection and the backend write remain serialized. Callers must handle
short writes.
This does not introduce a writeback cache or change `file_sync` semantics.

Machine-visible buffers use the HAL's non-faulting external-memory copy
contract, not Rust references that assume exclusive access. AArch64 copies
8-byte-aligned ranges with GPR pairs, then whole words and an exact byte tail;
unaligned ranges retain the byte path. No access crosses the validated range,
uses SIMD state, supplies a snapshot, or replaces the owner's publication
barriers. MMIO is excluded from this Normal-memory contract.

The actual AArch64 copy instructions run in host tests on AArch64, including
all source/destination alignment combinations and protected-page boundaries
on macOS. QEMU storage and Native user-copy acceptance cover their callers.
Physical VHE-board validation must additionally exercise simultaneous Native
and I/O-VM transfers, aligned/unaligned buffers, checksums and stop/restart;
host/QEMU tests do not establish a real device's DMA or cache-coherence behavior.

## Rooted directory scopes

`directory_scope_create` combines an explicit root location and a reachable
starting directory. Both handles must authorize every requested right. A scope
owns its root and current node independently of the input handles. Ordinary
child Directory handles remain confined to their own subtree; scopes support
relative paths from a retained cwd while absolute paths and absolute symlinks
restart at the explicit root.

Resolution reconstructs current ancestry from validated forward edges and
checks a namespace mutation epoch. A renamed cwd therefore follows the same
node, while detached contextual locations are rejected. Traversal is iterative,
with bounded path bytes, depth and symlink expansion. A writer in progress is
waited for through the namespace mutex; repeated concurrent changes can return
`Busy`. Canonical paths describe the observed spelling, not a new authority.

The initial root Directory has an explicit namespace-root anchor. Duplicating
it, or deriving a scope from it, preserves that anchor; opening a child Directory
instead fixes the capability to the selected location. Namespace-root anchors
follow `filesystem_switch_root`, while fixed roots keep their confinement and
existing files keep their node, mount and cache identities.

`filesystem_mount_ramfs` prepares an independent memory filesystem.
Internal RAM subtree bindings support root-switch preparation without a public
bind-mount syscall or aliases of remote workers with independent retirement.
`filesystem_switch_root` atomically changes the root and its mount topology,
attaches the old root at the caller-specified destination and moves the explicitly
listed mounts or RAM subtrees. A RAM subtree becomes a mount with a new `MountId`;
its old directory entry is removed in the same transaction, while filesystem
nodes and cached file contents are retained. Mounted descendants below a promoted
RAM subtree and moves whose source overlaps that subtree are rejected. The new root must be the
root of another mounted filesystem; at most sixteen moves are accepted.
New-root child mounts remain attached;
other old mounts remain below the old root. All three namespace operations
require `MANAGE_NAMESPACE | WRITE | EXECUTE` on a Directory whose traversal root
and current location both follow the namespace root. The initial init receives
this authority; ordinary directory grants do not confer it.
Move destinations must be distinct directories in the new root filesystem,
outside its existing child mounts, and cannot cover each other or those mounts.
The new-root path and move sources resolve in the old namespace. The old-root
attachment and move destinations resolve relative to the new root, including
when written with a leading slash.

All path validation, allocation and mountpoint pinning precede publication.
Affected filesystem mutation locks are acquired in `FilesystemId` order. RAM
subtree detachment and the topology replacement publish within their mutation
epochs; a failed preparation leaves the old directories and topology intact. A
concurrent topology change returns `Busy` without partial moves. No spinlock
spans backend I/O, and switching roots does not flush, unmount or restart a
filesystem worker. Promoting a RAM subtree preserves existing File capabilities
and scopes whose fixed root is inside that subtree. Each RAM subtree mount owns
its root independently of directory links, so the promoted root remains writable.
A namespace-root scope whose cwd retains a location inside the promoted subtree
on its old mount becomes detached: relative lookup fails, while absolute lookup
still follows its namespace root. Cwds outside promoted subtrees retain their
paths below the relocated old root. Paths such as `/boot` and `/system` are
supplied by userspace.

Final-component nofollow opens and identity-conditioned removal support safe
recursive deletion. Each traversal pins its parent Directory and uses a child
basename, so replacing a name with a symlink cannot redirect deletion outside
the pinned authority. Concurrent replacement may make deletion fail rather
than delete a different identity. An already open File remains usable after
its last name is removed.

Temporary path storage is charged to the process issuing the request, not to
the creator of its Directory capability. Buffers grow fallibly with actual
path depth and retained name bytes; there is no maximum-depth allocation or
maximum-size quota reservation for every open. Growth admits both old and new
capacity before replacing storage. Each retry releases its own buffers, and
returned paths retain their charges until userspace copyout finishes.

## Content protection

`PROTECT` authorizes an explicit content-protection acquisition; it does not
imply `READ`, `WRITE`, `EXECUTE`, or an active lock. `file_protect` waits for the
current per-file operation and returns a process-local `FileProtection` token
and the stable file size. Multiple independent holders may coexist. Once
acquired, all writes, append, resize and truncating opens through the canonical
node return `Busy`, including already-open writable handles and hard-link
aliases. Metadata, names and advisory locks remain separate mechanisms.

Protection tokens support only `MAP | INSPECT`. They cannot be duplicated or
transferred. Ordinary File handles can carry attenuated `PROTECT` authority,
but moving or duplicating a File does not move an existing protection grant.
Removing `PROTECT` from a File prevents further acquisitions without revoking
already granted protection. Closing the token releases its own hold even when
an internal operation/inspection pin retains the token object's allocation.

A file mapping verifies that the token protects the exact canonical content
and that its destination VMAR belongs to the calling process. It retains its
own guard, so closing the token cannot permit a writer to change pages needed
by an existing mapping. Once all of a process's threads are quiescent and its
handles close, teardown drops its file-source guards without allocating or
waiting for escaped VMAR references. Those references may retain requester-paid
pages, but cannot preserve protection or refill from the file. Another process's
independent protection is unaffected.
Acquisition and handle publication roll back if allocation, quota or concurrent
process exit rejects the token. Guard metadata is charged to the requester;
a guard retains no file-data pages and does not trigger a whole-file read.

Protected file mappings retain a mount/node lease. Ordinary clean cache entries
remain reclaimable; mapping-page retirement separately revokes translations
before releasing the physical page. A subsequent fault reads the same protected
file range. Mapping pages are currently requester-charged copies populated from
the shared read cache, not shared physical mappings of its cache pages. Both
clean copies can be reclaimed independently. Modified private pages cannot be
recovered from the file and remain
resident in the absence of swap. The protection keeps contents stable, not pages
resident. Protection is only meaningful for media whose writers all pass through
the authoritative filesystem service; out-of-band raw media writes violate the
filesystem contract.

## Advisory file locks

Each canonical node owns one whole-file advisory lock domain. Separate opens
have distinct owners; duplicated or transferred handles retain their open
instance's owner. Locks do not prevent ordinary data I/O by uncooperative
holders. Shared and exclusive acquisition uses FIFO order, granting a shared
prefix together without allowing new readers to bypass a waiting writer.

Each owner permits one pending acquisition. A second request returns `Busy`
until the earlier continuation retires; unlock also cancels a pending request.
With no pending continuation, repeated acquisition in the same mode is idempotent. Exclusive-to-shared
conversion is atomic. Shared-to-exclusive conversion succeeds only for a sole
holder with no predecessor; otherwise it preserves the shared lock and returns
`WouldBlock` for a try operation or `Busy` for a blocking request. It never
waits while retaining a shared lock needed by another upgrading owner.

Waits park through the scheduler with monotonic deadlines and cancellation.
The final active handle releases its owner's grant immediately, even if an
operation pin keeps the object alive. Last-close callbacks do not allocate,
block, or call a filesystem backend. Pending records are quota charged before
publication and bounded to 256 per node, including records awaiting retirement.

## Filesystem time

Metadata carries signed Unix seconds and normalized nanoseconds, with validity
bits for individual timestamps. The boot archive supplies modification times.
New mutations use the kernel wall clock. Without an RTC this is an
uncalibrated Unix epoch plus uptime baseline, shared with `SystemTime::now()`.
An unavailable monotonic clock still leaves timestamps unavailable. Native callers can set
access and modification times, including times before the Unix epoch.

The [UTC clock](time.md) is independent of filesystem policy. Filesystems
choose timestamp resolution and persistence; ramfs retains supplied timestamp
precision in memory.
