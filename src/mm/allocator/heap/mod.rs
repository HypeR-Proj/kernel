// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime heap mechanisms and allocation accounting.
//!
//! This module owns slab, large-allocation, and direct-page metadata over one
//! buddy allocator. The private `global` module adapts that mechanism to
//! Rust's process-wide allocation ABI and owns initialization/locking policy.
//! Consumer lifetime policy remains above this layer; `PageOwner` is diagnostic
//! accounting, not an ownership token.

pub(crate) mod cache_snapshot;
pub(crate) mod global;
pub(crate) mod local_cache;
pub(crate) mod partial;
pub(crate) mod profile;

use core::alloc::Layout;
use core::fmt;
use core::ptr::{NonNull, null_mut};

use hyper::sync::PublishedOnce;
use {hyper::sync::atomic::AtomicU8, hyper::sync::atomic::Ordering};

use {
    crate::mm::allocator::BuddyAllocator, crate::mm::allocator::BuddyError,
    crate::mm::allocator::BuddyStats, crate::mm::allocator::MAX_ORDER, hyper::mm::PAGE_SIZE,
    hyper::mm::PhysicalAddress, hyper::mm::boot::MemoryHandoff,
};

use {
    crate::mm::allocator::heap::partial::InsertPermit,
    crate::mm::allocator::heap::partial::PartialLinks,
    crate::mm::allocator::heap::partial::PartialNode,
    crate::mm::allocator::heap::partial::PartialNodeStore,
    crate::mm::allocator::heap::partial::PartialSlabLists,
    crate::mm::allocator::heap::partial::RemovePermit,
    crate::mm::allocator::heap::partial::SlabClass, crate::mm::allocator::heap::partial::SlabLink,
    crate::mm::allocator::heap::partial::SlabPageId,
};

pub use {
    crate::mm::allocator::heap::global::CacheActivationError,
    crate::mm::allocator::heap::global::CacheMetadataReservation,
    crate::mm::allocator::heap::global::CpuLocalCachePolicy,
    crate::mm::allocator::heap::global::KernelGlobalAllocator,
    crate::mm::allocator::heap::global::PageAvailability,
};

const SLAB_MAGIC: u64 = 0x4859_5045_5253_4c42;
const LARGE_MAGIC: u64 = 0x4859_5045_524c_4152;
pub(super) const CLASS_SIZES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
const SLAB_CLASS_COUNT: usize = CLASS_SIZES.len();
const SLAB_DETACHED: u8 = 0;
const SLAB_LINKED: u8 = 1;

type HeapSlabClass = SlabClass<SLAB_CLASS_COUNT>;
type HeapInsertPermit<'a> = InsertPermit<'a, SLAB_CLASS_COUNT>;
type HeapRemovePermit<'a> = RemovePermit<'a, SLAB_CLASS_COUNT>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LargeAllocationError {
    OutOfMemory,
    UnsupportedLayout,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum AllocatorFault {
    InvalidSlabHeader = 1,
    SlabClassMismatch = 2,
    InvalidSlabPointer = 3,
    AllocationCountUnderflow = 4,
    InvalidPartialList = 5,
    BuddyDeallocation = 6,
    InvalidLargeHeader = 7,
    PageOwnerUnderflow = 8,
    InvalidSlabPhysical = 9,
    AllocationCountOverflow = 10,
    InvalidLargePointer = 11,
    UninitializedDeallocation = 12,
    InvalidCacheState = 13,
    CachePolicyFailure = 14,
    CacheAccountingUnderflow = 15,
    CacheAccountingOverflow = 16,
}

static LAST_ALLOCATOR_FAULT: AtomicU8 = AtomicU8::new(0);

/// Opaque identity of one violated runtime-allocator invariant.
///
/// The numeric code is stable for debugger and crash-report inspection, while
/// allocator implementation details remain private to this module.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct AllocatorInvariant(u8);

impl AllocatorInvariant {
    /// Recovers a currently defined invariant from its stable numeric code.
    ///
    /// Numeric recovery is public because crash dumps and debuggers retain the
    /// code without depending on private allocator implementation types.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1..=16 => Some(Self(code)),
            _ => None,
        }
    }

    pub const fn code(self) -> u8 {
        self.0
    }

    pub const fn description(self) -> &'static str {
        match self.0 {
            1 => "invalid slab header",
            2 => "slab size-class mismatch",
            3 => "invalid slab allocation pointer",
            4 => "allocation accounting underflow",
            5 => "invalid partial-slab list",
            6 => "buddy deallocation failure",
            7 => "invalid large-allocation header",
            8 => "page-owner accounting underflow",
            9 => "invalid slab physical address",
            10 => "allocation accounting overflow",
            11 => "invalid large-allocation pointer",
            12 => "deallocation before allocator initialization",
            13 => "invalid CPU-local allocator cache state",
            14 => "CPU-local allocator cache policy failure",
            15 => "CPU-local allocator accounting underflow",
            16 => "CPU-local allocator accounting overflow",
            _ => "unknown allocator invariant",
        }
    }
}

impl fmt::Debug for AllocatorInvariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AllocatorInvariant")
            .field("code", &self.code())
            .field("description", &self.description())
            .finish()
    }
}

/// One allocator failure and the first failure retained for post-mortem use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorInvariantReport {
    current: AllocatorInvariant,
    first: AllocatorInvariant,
}

impl AllocatorInvariantReport {
    const fn new(current: AllocatorInvariant, first: AllocatorInvariant) -> Self {
        Self { current, first }
    }

    pub const fn current(self) -> AllocatorInvariant {
        self.current
    }

    pub const fn first(self) -> AllocatorInvariant {
        self.first
    }
}

/// Non-returning policy callback for corrupted allocator state.
///
/// The handler may be called with local IRQs masked and allocator or unrelated
/// outer locks held. It must not allocate, deallocate, block, acquire ordinary
/// locks, unwind, or return to the corrupted allocator transaction.
pub type AllocatorInvariantHandler = fn(AllocatorInvariantReport) -> !;

/// Failure to publish the process-wide allocator corruption policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocatorInvariantInstallError {
    AlreadyInstalled,
}

struct AllocatorInvariantHandlerSlot {
    handler: PublishedOnce<AllocatorInvariantHandler>,
}

impl AllocatorInvariantHandlerSlot {
    const fn new() -> Self {
        Self {
            handler: PublishedOnce::new(),
        }
    }

    fn install(
        &self,
        handler: AllocatorInvariantHandler,
    ) -> Result<(), AllocatorInvariantInstallError> {
        self.handler
            .publish(handler)
            .map_err(|_| AllocatorInvariantInstallError::AlreadyInstalled)
    }

    fn get(&self) -> Option<AllocatorInvariantHandler> {
        self.handler.get().copied()
    }
}

static ALLOCATOR_INVARIANT_HANDLER: AllocatorInvariantHandlerSlot =
    AllocatorInvariantHandlerSlot::new();

/// Installs the process-wide allocator corruption policy exactly once.
pub fn install_allocator_invariant_handler(
    handler: AllocatorInvariantHandler,
) -> Result<(), AllocatorInvariantInstallError> {
    ALLOCATOR_INVARIANT_HANDLER.install(handler)
}

/// Returns zero or the stable numeric code of the first allocator fault.
pub fn allocator_fault_code() -> u8 {
    LAST_ALLOCATOR_FAULT.load(Ordering::Acquire)
}

fn allocator_fault(fault: AllocatorFault) -> ! {
    let current = AllocatorInvariant(fault as u8);
    let first_code = match LAST_ALLOCATOR_FAULT.compare_exchange(
        0,
        current.code(),
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => current.code(),
        Err(first) => first,
    };
    let Some(first) = AllocatorInvariant::from_code(first_code) else {
        hyper::debug::invariant_failure("invalid allocator fault record (see LAST_ALLOCATOR_FAULT)")
    };
    let report = AllocatorInvariantReport::new(current, first);
    if let Some(handler) = ALLOCATOR_INVARIANT_HANDLER.get() {
        handler(report)
    }
    hyper::debug::invariant_failure(
        "allocator failure before handler installation (see LAST_ALLOCATOR_FAULT)",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitError {
    AlreadyInitialized,
    Buddy(BuddyError),
}

impl From<BuddyError> for InitError {
    fn from(error: BuddyError) -> Self {
        Self::Buddy(error)
    }
}

/// Subsystem ownership for allocations made directly from the page allocator.
///
/// Heap backing pages are accounted separately. `Guest` means physically
/// committed guest memory, not the guest's advertised address-space size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PageOwner {
    Kernel = 0,
    PageTable = 1,
    Guest = 2,
    User = 3,
    FileCache = 4,
}

impl PageOwner {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageOwnerStats {
    pub pages: usize,
    pub peak_pages: usize,
    pub allocation_requests: u64,
    pub allocation_failures: u64,
    pub deallocations: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeapStats {
    pub buddy: BuddyStats,
    /// Compatibility shortcut for `buddy.free_pages`.
    pub free_pages: usize,
    pub slab_pages: usize,
    pub large_heap_pages: usize,
    /// Subsets of heap backing owned exclusively by file-cache metadata.
    pub file_cache_slab_pages: usize,
    pub file_cache_large_pages: usize,
    pub live_allocations: usize,
    pub live_slab_allocations: usize,
    pub live_large_allocations: usize,
    pub peak_live_allocations: usize,
    pub requested_bytes: usize,
    pub peak_requested_bytes: usize,
    pub allocation_requests: u64,
    pub allocation_failures: u64,
    pub cache: HeapCacheStats,
    pub kernel_pages: PageOwnerStats,
    pub page_table_pages: PageOwnerStats,
    pub guest_pages: PageOwnerStats,
    pub user_pages: PageOwnerStats,
    /// Subset of user pages retained by exclusive guest-memory leases.
    pub guest_loaned_pages: usize,
    pub file_cache_pages: PageOwnerStats,
}

/// Diagnostic state for the bounded CPU-local slab magazines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeapCacheStats {
    pub enabled_cpus: usize,
    pub cached_objects: usize,
    /// Complete pages recoverable by draining cached tokens; None on contention.
    pub reclaimable_pages: Option<usize>,
    pub hits: u64,
    pub misses: u64,
    pub refills: u64,
    pub drains: u64,
    pub pressure_reclaims: u64,
    pub reclaimed_objects: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct SlabHeader {
    magic: u64,
    physical: u64,
    previous_partial: u64,
    next_partial: u64,
    free_head: usize,
    in_use: u16,
    capacity: u16,
    class: u8,
    list_state: u8,
    pool: u8,
    _reserved: u8,
}

const _: () = assert!(core::mem::size_of::<SlabHeader>() == 48);
const _: () = assert!(PAGE_SIZE == crate::mm::allocator::heap::partial::SLAB_PAGE_SIZE);

#[repr(C)]
#[derive(Clone, Copy)]
struct LargeHeader {
    magic: u64,
    physical: u64,
    order: u8,
    pool: u8,
    _reserved: [u8; 6],
}

/// Separate slab topology prevents cache metadata from stranding pages occupied
/// by long-lived ordinary kernel objects. Both pools use the same buddy owner.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
enum HeapPool {
    Kernel = 0,
    FileCache = 1,
}
impl HeapPool {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Kernel,
            1 => Self::FileCache,
            _ => allocator_fault(AllocatorFault::InvalidSlabHeader),
        }
    }
}

/// # Safety
/// The pointer/layout must identify a live allocation in this heap. Only the
/// immutable pool byte is read; mutable slab header fields are not borrowed.
unsafe fn allocation_pool(pointer: *mut u8, layout: Layout) -> HeapPool {
    let pool = if slab_class_for_layout(layout).is_some() {
        let header = ((pointer as usize) & !(PAGE_SIZE as usize - 1)) as *const SlabHeader;
        // SAFETY: The live slab object retains its initialized, immutable tag.
        unsafe { core::ptr::addr_of!((*header).pool).read() }
    } else {
        // SAFETY: Large allocations have the exact header immediately before them.
        let header =
            unsafe { pointer.sub(core::mem::size_of::<LargeHeader>()) }.cast::<LargeHeader>();
        // SAFETY: The caller owns this live allocation and its immutable tag.
        unsafe { core::ptr::addr_of!((*header).pool).read() }
    };
    HeapPool::from_raw(pool)
}

/// Linear ownership of one object reserved from the central slab topology.
///
/// A token may reside in one transfer batch or CPU-local magazine. Converting
/// it to a caller pointer relinquishes this internal ownership; reconstructing
/// a token is permitted only by the matching `GlobalAlloc::dealloc` contract.
struct CachedObject {
    pointer: NonNull<u8>,
    class: HeapSlabClass,
    armed: bool,
}

// SAFETY: A CachedObject uniquely owns untyped allocator storage. It is never
// dereferenced while cached, and the central slab topology keeps its backing
// page resident until the token is returned.
unsafe impl Send for CachedObject {}

impl CachedObject {
    fn new(pointer: NonNull<u8>, class: HeapSlabClass) -> Self {
        Self {
            pointer,
            class,
            armed: true,
        }
    }

    /// Adopts the exact live object relinquished by a `GlobalAlloc` caller.
    ///
    /// # Safety
    ///
    /// `pointer` must identify one live allocation from `class`, and the caller
    /// must no longer access it after this transfer.
    unsafe fn from_caller(pointer: NonNull<u8>, class: HeapSlabClass) -> Self {
        Self::new(pointer, class)
    }

    fn class(&self) -> HeapSlabClass {
        self.class
    }

    fn into_caller_pointer(mut self) -> *mut u8 {
        self.armed = false;
        self.pointer.as_ptr()
    }

    fn into_central_pointer(mut self) -> NonNull<u8> {
        self.armed = false;
        self.pointer
    }
}

impl Drop for CachedObject {
    fn drop(&mut self) {
        if self.armed {
            allocator_fault(AllocatorFault::InvalidCacheState);
        }
    }
}

/// Owns a buddy block until the surrounding heap metadata is publishable.
///
/// Slab and large allocations both perform fallible address arithmetic after
/// obtaining physical memory. Keeping that memory in this guard makes every
/// pre-publication return path restore the buddy allocator automatically.
struct PendingBuddyBlock<'a> {
    buddy: &'a mut BuddyAllocator,
    physical: PhysicalAddress,
    order: usize,
    published: bool,
}

impl<'a> PendingBuddyBlock<'a> {
    fn allocate(buddy: &'a mut BuddyAllocator, order: usize) -> Result<Self, BuddyError> {
        let physical = buddy.allocate(order)?;
        Ok(Self {
            buddy,
            physical,
            order,
            published: false,
        })
    }

    fn physical(&self) -> PhysicalAddress {
        self.physical
    }

    fn virtual_address(&self) -> Option<usize> {
        self.buddy
            .direct_map_base()
            .checked_add(self.physical.get())
            .and_then(|address| usize::try_from(address).ok())
    }

    fn publish(mut self) -> PhysicalAddress {
        self.published = true;
        self.physical
    }
}

impl Drop for PendingBuddyBlock<'_> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        // SAFETY: An unpublished guard uniquely owns the exact buddy block
        // acquired by `allocate`; no pointer to it has escaped.
        if unsafe { self.buddy.deallocate(self.physical, self.order) }.is_err() {
            allocator_fault(AllocatorFault::BuddyDeallocation);
        }
    }
}

#[derive(Clone, Copy)]
struct ValidatedSlab {
    pointer: NonNull<SlabHeader>,
    header: SlabHeader,
    class: HeapSlabClass,
    object_start: usize,
    object_end: usize,
}

#[derive(Clone, Copy)]
struct SlabHeaderAccess<'a> {
    buddy: &'a BuddyAllocator,
}

struct SlabTopology<'a> {
    headers: SlabHeaderAccess<'a>,
}

impl PartialNodeStore<SLAB_CLASS_COUNT> for SlabTopology<'_> {
    type Error = ();

    fn resolve(&self, page: SlabPageId) -> Result<PartialNode<SLAB_CLASS_COUNT>, Self::Error> {
        let slab = self.headers.validated_slab(page).ok_or(())?;
        Ok(PartialNode {
            class: slab.class,
            links: PartialLinks {
                previous: SlabLink::from_raw(slab.header.previous_partial),
                next: SlabLink::from_raw(slab.header.next_partial),
            },
            linked: slab.header.list_state == SLAB_LINKED,
        })
    }
}

struct PreparedInsert<'a> {
    permit: HeapInsertPermit<'a>,
    target: NonNull<SlabHeader>,
    old_head: Option<NonNull<SlabHeader>>,
}

struct PreparedRemove<'a> {
    permit: HeapRemovePermit<'a>,
    target: NonNull<SlabHeader>,
    previous: Option<NonNull<SlabHeader>>,
    next: Option<NonNull<SlabHeader>>,
}

enum PreparedTransition<'a> {
    None,
    Insert(PreparedInsert<'a>),
    Remove(PreparedRemove<'a>),
}

impl PreparedTransition<'_> {
    fn commit(self) {
        match self {
            Self::None => {}
            Self::Insert(prepared) => prepared.commit(),
            Self::Remove(prepared) => prepared.commit(),
        }
    }
}

impl SlabHeaderAccess<'_> {
    fn validated_slab(self, page: SlabPageId) -> Option<ValidatedSlab> {
        let page_virtual = self.buddy.managed_page_pointer(page.physical()).ok()?;
        let pointer = NonNull::new(core::ptr::with_exposed_provenance_mut::<SlabHeader>(
            page_virtual,
        ))?;
        // SAFETY: managed_page_pointer proved that a complete handed-off page
        // is direct-mapped. Slab links are minted only for live slab pages;
        // without a separate ownership bitmap, arbitrary corrupted links into
        // a concurrently used non-slab page are outside this invariant.
        let header = unsafe { pointer.as_ptr().read() };
        if header.magic != SLAB_MAGIC || header.physical != page.physical() {
            return None;
        }
        let class = HeapSlabClass::new(usize::from(header.class))?;
        let object_size = CLASS_SIZES[class.index()];
        let object_start = page_virtual
            .checked_add(core::mem::size_of::<SlabHeader>())
            .and_then(|end| align_up_usize(end, object_size))?;
        let page_end = page_virtual.checked_add(PAGE_SIZE as usize)?;
        let expected_capacity = (page_end - object_start) / object_size;
        if expected_capacity == 0
            || expected_capacity > u16::MAX as usize
            || usize::from(header.capacity) != expected_capacity
            || header.in_use > header.capacity
        {
            return None;
        }
        let object_end = object_start.checked_add(expected_capacity.checked_mul(object_size)?)?;
        let linked = header.list_state == SLAB_LINKED;
        let detached = header.list_state == SLAB_DETACHED;
        if (!linked && !detached)
            || (linked && header.in_use == header.capacity)
            || (detached
                && (header.in_use != header.capacity
                    || header.previous_partial != SlabLink::NONE.raw()
                    || header.next_partial != SlabLink::NONE.raw()))
            || (header.free_head == 0) != (header.in_use == header.capacity)
        {
            return None;
        }
        let slab = ValidatedSlab {
            pointer,
            header,
            class,
            object_start,
            object_end,
        };
        if header.free_head != 0 && !self.valid_free_object(slab, header.free_head) {
            return None;
        }
        Some(slab)
    }

    fn valid_free_object(self, slab: ValidatedSlab, address: usize) -> bool {
        let object_size = CLASS_SIZES[slab.class.index()];
        address >= slab.object_start
            && address < slab.object_end
            && (address - slab.object_start).is_multiple_of(object_size)
    }

    fn read_free_link(self, slab: ValidatedSlab, address: usize) -> Option<usize> {
        if address == 0 || !self.valid_free_object(slab, address) {
            return None;
        }
        // SAFETY: address was validated as one object in this slab's free
        // extent. External allocator locking excludes concurrent free-list
        // mutation, and the linked object stores an initialized usize.
        let next = unsafe { core::ptr::with_exposed_provenance::<usize>(address).read() };
        if next == 0 || self.valid_free_object(slab, next) {
            Some(next)
        } else {
            None
        }
    }
}

impl<'a> PreparedInsert<'a> {
    fn new(permit: HeapInsertPermit<'a>, headers: SlabHeaderAccess<'_>) -> Option<Self> {
        let target = headers.validated_slab(permit.target())?.pointer;
        let old_head = match permit.old_head() {
            Some(page) => Some(headers.validated_slab(page)?.pointer),
            None => None,
        };
        Some(Self {
            permit,
            target,
            old_head,
        })
    }

    fn commit(self) {
        let old_head = self
            .permit
            .old_head()
            .map_or(SlabLink::NONE, SlabLink::from_page)
            .raw();
        // SAFETY: preflight validated distinct, stable headers and every
        // mutation below is an infallible scalar store under exclusive access.
        unsafe {
            (*self.target.as_ptr()).previous_partial = SlabLink::NONE.raw();
            (*self.target.as_ptr()).next_partial = old_head;
            (*self.target.as_ptr()).list_state = SLAB_LINKED;
            if let Some(old_head) = self.old_head {
                (*old_head.as_ptr()).previous_partial = self.permit.target().physical();
            }
        }
        self.permit.commit();
    }
}

impl<'a> PreparedRemove<'a> {
    fn new(permit: HeapRemovePermit<'a>, headers: SlabHeaderAccess<'_>) -> Option<Self> {
        let target = headers.validated_slab(permit.target())?.pointer;
        let previous = match permit.previous() {
            Some(page) => Some(headers.validated_slab(page)?.pointer),
            None => None,
        };
        let next = match permit.next() {
            Some(page) => Some(headers.validated_slab(page)?.pointer),
            None => None,
        };
        Some(Self {
            permit,
            target,
            previous,
            next,
        })
    }

    fn commit(self) {
        let previous = self
            .permit
            .previous()
            .map_or(SlabLink::NONE, SlabLink::from_page)
            .raw();
        let next = self
            .permit
            .next()
            .map_or(SlabLink::NONE, SlabLink::from_page)
            .raw();
        // SAFETY: preflight validated reciprocal, distinct headers and every
        // mutation below is an infallible scalar store under exclusive access.
        unsafe {
            if let Some(previous_header) = self.previous {
                (*previous_header.as_ptr()).next_partial = next;
            }
            if let Some(next_header) = self.next {
                (*next_header.as_ptr()).previous_partial = previous;
            }
            (*self.target.as_ptr()).previous_partial = SlabLink::NONE.raw();
            (*self.target.as_ptr()).next_partial = SlabLink::NONE.raw();
            (*self.target.as_ptr()).list_state = SLAB_DETACHED;
        }
        self.permit.commit();
    }
}

pub struct SlabAllocator {
    #[cfg(feature = "heap-profiling")]
    profile_backing: [crate::mm::allocator::heap::profile::Backing;
        crate::mm::allocator::heap::profile::BUCKETS],
    buddy: BuddyAllocator,
    partial: [PartialSlabLists<SLAB_CLASS_COUNT>; 2],
    file_cache_slab_pages: usize,
    file_cache_large_pages: usize,
    slab_pages: usize,
    large_heap_pages: usize,
    live_allocations: usize,
    live_slab_allocations: usize,
    live_large_allocations: usize,
    peak_live_allocations: usize,
    requested_bytes: usize,
    peak_requested_bytes: usize,
    allocation_requests: u64,
    allocation_failures: u64,
    page_owners: [PageOwnerStats; PageOwner::COUNT],
    guest_loaned_pages: usize,
}

impl SlabAllocator {
    /// Creates a slab heap backed by an intrusive buddy allocator.
    ///
    /// # Safety
    ///
    /// The direct map must remain writable for the allocator's lifetime. Its
    /// base must preserve page alignment so a slab object address can be
    /// converted back to the physical page that owns its header.
    pub unsafe fn from_handoff(
        handoff: &MemoryHandoff,
        direct_map_base: u64,
    ) -> Result<Self, BuddyError> {
        if direct_map_base & (PAGE_SIZE - 1) != 0 {
            return Err(BuddyError::Unaddressable);
        }
        Ok(Self {
            #[cfg(feature = "heap-profiling")]
            profile_backing: [crate::mm::allocator::heap::profile::Backing::EMPTY;
                crate::mm::allocator::heap::profile::BUCKETS],
            // SAFETY: This constructor inherits and preserves its caller's
            // permanent writable direct-map contract for the buddy lifetime.
            buddy: unsafe { BuddyAllocator::from_handoff(handoff, direct_map_base)? },
            partial: [PartialSlabLists::new(), PartialSlabLists::new()],
            file_cache_slab_pages: 0,
            file_cache_large_pages: 0,
            slab_pages: 0,
            large_heap_pages: 0,
            live_allocations: 0,
            live_slab_allocations: 0,
            live_large_allocations: 0,
            peak_live_allocations: 0,
            requested_bytes: 0,
            peak_requested_bytes: 0,
            allocation_requests: 0,
            allocation_failures: 0,
            page_owners: [PageOwnerStats::default(); PageOwner::COUNT],
            guest_loaned_pages: 0,
        })
    }

    /// Allocates memory satisfying `layout`.
    pub fn allocate(&mut self, layout: Layout) -> *mut u8 {
        self.allocation_requests = self.allocation_requests.saturating_add(1);
        let class = self.slab_class_for(layout);
        let pointer = if let Some(class) = class {
            self.reserve_slab_object(class)
                .map_or(null_mut(), CachedObject::into_caller_pointer)
        } else {
            self.allocate_large(layout).unwrap_or(null_mut())
        };
        if pointer.is_null() {
            self.allocation_failures = self.allocation_failures.saturating_add(1);
        } else {
            increment(&mut self.live_allocations);
            if class.is_some() {
                increment(&mut self.live_slab_allocations);
            } else {
                increment(&mut self.live_large_allocations);
            }
            let requested = layout.size().max(1);
            self.requested_bytes = self.requested_bytes.saturating_add(requested);
            self.peak_requested_bytes = self.peak_requested_bytes.max(self.requested_bytes);
            self.peak_live_allocations = self.peak_live_allocations.max(self.live_allocations);
        }
        pointer
    }

    /// Releases a live allocation returned by `allocate`.
    ///
    /// # Safety
    ///
    /// `pointer` and `layout` must describe one live allocation from this heap.
    pub unsafe fn deallocate(&mut self, pointer: *mut u8, layout: Layout) {
        let requested_bytes = match self.requested_bytes.checked_sub(layout.size().max(1)) {
            Some(requested_bytes) => requested_bytes,
            None => allocator_fault(AllocatorFault::AllocationCountUnderflow),
        };
        let class = self.slab_class_for(layout);
        if self.live_allocations == 0
            || class.is_some() && self.live_slab_allocations == 0
            || class.is_none() && self.live_large_allocations == 0
        {
            allocator_fault(AllocatorFault::AllocationCountUnderflow);
        }
        if let Some(class) = class {
            let Some(pointer) = NonNull::new(pointer) else {
                allocator_fault(AllocatorFault::InvalidSlabPointer);
            };
            // SAFETY: The caller's exact pointer/layout contract identifies a
            // live object in this computed slab size class.
            let cached = unsafe { CachedObject::from_caller(pointer, class) };
            self.release_slab_object(cached);
            self.live_slab_allocations -= 1;
        } else {
            // SAFETY: A non-slab layout was allocated with an adjacent valid
            // LargeHeader and the caller relinquishes that live allocation.
            unsafe { self.deallocate_large(pointer, layout) };
            self.live_large_allocations -= 1;
        }
        self.live_allocations -= 1;
        self.requested_bytes = requested_bytes;
    }

    fn reserve_slab_object(&mut self, class: HeapSlabClass) -> Option<CachedObject> {
        self.reserve_slab_object_in(class, HeapPool::Kernel)
    }

    fn reserve_slab_object_in(
        &mut self,
        class: HeapSlabClass,
        pool: HeapPool,
    ) -> Option<CachedObject> {
        if self.partial[pool as usize].head(class).is_none()
            && self.create_slab(class, pool).is_err()
        {
            return None;
        }

        let page = match self.partial[pool as usize].head(class) {
            Some(page) => page,
            None => allocator_fault(AllocatorFault::InvalidPartialList),
        };
        let headers = SlabHeaderAccess { buddy: &self.buddy };
        let slab = match headers.validated_slab(page) {
            Some(slab) => slab,
            None => allocator_fault(AllocatorFault::InvalidPartialList),
        };
        if slab.class != class
            || slab.header.list_state != SLAB_LINKED
            || HeapPool::from_raw(slab.header.pool) != pool
        {
            allocator_fault(AllocatorFault::SlabClassMismatch);
        }
        let pointer = slab.header.free_head;
        let next_free = match headers.read_free_link(slab, pointer) {
            Some(next) => next,
            None => allocator_fault(AllocatorFault::InvalidPartialList),
        };
        let in_use = match slab.header.in_use.checked_add(1) {
            Some(in_use) => in_use,
            None => allocator_fault(AllocatorFault::AllocationCountOverflow),
        };
        let becomes_full = next_free == 0;
        if becomes_full != (in_use == slab.header.capacity) {
            allocator_fault(AllocatorFault::InvalidPartialList);
        }
        let removal = if becomes_full {
            let permit = match self.partial[pool as usize].preflight_remove(
                &SlabTopology { headers },
                class,
                page,
            ) {
                Ok(permit) => permit,
                Err(_) => allocator_fault(AllocatorFault::InvalidPartialList),
            };
            match PreparedRemove::new(permit, headers) {
                Some(prepared) => Some(prepared),
                None => allocator_fault(AllocatorFault::InvalidPartialList),
            }
        } else {
            None
        };

        // SAFETY: Every address and counter involved in this transaction was
        // validated above, and external allocator locking excludes mutation.
        unsafe {
            (*slab.pointer.as_ptr()).free_head = next_free;
            (*slab.pointer.as_ptr()).in_use = in_use;
        }
        if let Some(removal) = removal {
            removal.commit();
        }
        #[cfg(feature = "heap-profiling")]
        {
            self.profile_backing[crate::mm::allocator::heap::profile::bucket(pool, Some(class))]
                .reserved += 1;
        }
        let Some(pointer) = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(pointer))
        else {
            allocator_fault(AllocatorFault::InvalidSlabPointer);
        };
        Some(CachedObject::new(pointer, class))
    }

    // The central heap lock and the borrowed cached token keep this header
    // resident and immutable throughout the census read.
    fn cached_page_reservations(&self, object: &CachedObject) -> Option<(u64, usize)> {
        let page_virtual = (object.pointer.as_ptr() as usize) & !(PAGE_SIZE as usize - 1);
        let physical = (page_virtual as u64).checked_sub(self.buddy.direct_map_base())?;
        let page = SlabPageId::new(physical)?;
        let slab = (SlabHeaderAccess { buddy: &self.buddy }).validated_slab(page)?;
        if slab.class != object.class() {
            return None;
        }
        Some((physical, usize::from(slab.header.in_use)))
    }

    fn release_slab_object(&mut self, object: CachedObject) {
        let class = object.class();
        let pointer = object.into_central_pointer().as_ptr();
        let page_virtual = (pointer as usize) & !(PAGE_SIZE as usize - 1);
        let physical = match (page_virtual as u64).checked_sub(self.buddy.direct_map_base()) {
            Some(physical) => physical,
            None => allocator_fault(AllocatorFault::InvalidSlabPointer),
        };
        let page = match SlabPageId::new(physical) {
            Some(page) => page,
            None => allocator_fault(AllocatorFault::InvalidSlabPointer),
        };
        let headers = SlabHeaderAccess { buddy: &self.buddy };
        let slab = match headers.validated_slab(page) {
            Some(slab) => slab,
            None => allocator_fault(AllocatorFault::InvalidSlabPointer),
        };
        if slab.class != class {
            allocator_fault(AllocatorFault::SlabClassMismatch);
        }
        let pool = HeapPool::from_raw(slab.header.pool);
        let pointer_address = pointer as usize;
        if !headers.valid_free_object(slab, pointer_address) {
            allocator_fault(AllocatorFault::InvalidSlabPointer);
        }
        if slab.header.in_use == 0 {
            allocator_fault(AllocatorFault::AllocationCountUnderflow);
        }
        let in_use = slab.header.in_use - 1;
        let was_full = slab.header.list_state == SLAB_DETACHED;
        let capacity_one_empty = was_full && slab.header.capacity == 1;
        let transition = if was_full && !capacity_one_empty {
            let permit = match self.partial[pool as usize].preflight_insert(
                &SlabTopology { headers },
                class,
                page,
            ) {
                Ok(permit) => permit,
                Err(_) => allocator_fault(AllocatorFault::InvalidPartialList),
            };
            match PreparedInsert::new(permit, headers) {
                Some(prepared) => PreparedTransition::Insert(prepared),
                None => allocator_fault(AllocatorFault::InvalidPartialList),
            }
        } else if !was_full && in_use == 0 {
            let permit = match self.partial[pool as usize].preflight_remove(
                &SlabTopology { headers },
                class,
                page,
            ) {
                Ok(permit) => permit,
                Err(_) => allocator_fault(AllocatorFault::InvalidPartialList),
            };
            match PreparedRemove::new(permit, headers) {
                Some(prepared) => PreparedTransition::Remove(prepared),
                None => allocator_fault(AllocatorFault::InvalidPartialList),
            }
        } else {
            PreparedTransition::None
        };
        if self.slab_pages == 0 {
            allocator_fault(AllocatorFault::AllocationCountUnderflow);
        }

        // SAFETY: The caller relinquishes this live slab object, whose size is
        // at least one word; it is now exclusive free-list storage.
        unsafe {
            pointer.cast::<usize>().write(slab.header.free_head);
            (*slab.pointer.as_ptr()).free_head = pointer_address;
            (*slab.pointer.as_ptr()).in_use = in_use;
        }
        transition.commit();
        #[cfg(feature = "heap-profiling")]
        {
            self.profile_backing[crate::mm::allocator::heap::profile::bucket(pool, Some(class))]
                .reserved -= 1;
        }
        if in_use == 0 {
            // A capacity-one slab transitions directly from detached/full to
            // empty; it never enters the partial list only to leave it again.
            // SAFETY: The validated page is empty and exclusively owned by
            // this allocator until the buddy deallocation below.
            unsafe { (*slab.pointer.as_ptr()).magic = 0 };
            self.slab_pages -= 1;
            #[cfg(feature = "heap-profiling")]
            {
                let backing = &mut self.profile_backing
                    [crate::mm::allocator::heap::profile::bucket(pool, Some(class))];
                backing.pages -= 1;
                backing.capacity -= usize::from(slab.header.capacity);
            }
            if pool == HeapPool::FileCache {
                self.file_cache_slab_pages -= 1;
            }
            // SAFETY: The empty slab page is exclusively owned by this heap.
            if unsafe {
                self.buddy
                    .deallocate(PhysicalAddress::new(page.physical()), 0)
            }
            .is_err()
            {
                allocator_fault(AllocatorFault::BuddyDeallocation);
            }
        }
    }

    fn create_slab(&mut self, class: HeapSlabClass, pool: HeapPool) -> Result<(), BuddyError> {
        let headers = SlabHeaderAccess { buddy: &self.buddy };
        let head_permit =
            match self.partial[pool as usize].preflight_head(&SlabTopology { headers }, class) {
                Ok(permit) => permit,
                Err(_) => allocator_fault(AllocatorFault::InvalidPartialList),
            };
        let old_head = head_permit.head();
        let old_head_pointer = match old_head {
            Some(head) => match headers.validated_slab(head) {
                Some(slab) => Some(slab.pointer),
                None => allocator_fault(AllocatorFault::InvalidPartialList),
            },
            None => None,
        };
        let slab_pages = match self.slab_pages.checked_add(1) {
            Some(pages) => pages,
            None => allocator_fault(AllocatorFault::AllocationCountOverflow),
        };
        let pending = PendingBuddyBlock::allocate(&mut self.buddy, 0)?;
        let physical = pending.physical().get();
        let page = match SlabPageId::new(physical) {
            Some(page) => page,
            None => allocator_fault(AllocatorFault::InvalidSlabPhysical),
        };
        let permit = match head_permit.prepare_new_page(page) {
            Some(permit) => permit,
            None => allocator_fault(AllocatorFault::InvalidPartialList),
        };
        let header_pointer = pending
            .virtual_address()
            .map(core::ptr::with_exposed_provenance_mut::<SlabHeader>)
            .ok_or(BuddyError::Unaddressable)?;
        let object_size = CLASS_SIZES[class.index()];
        let page_virtual = header_pointer as usize;
        let object_start = page_virtual
            .checked_add(core::mem::size_of::<SlabHeader>())
            .and_then(|header_end| align_up_usize(header_end, object_size))
            .ok_or(BuddyError::Unaddressable)?;
        let object_offset = object_start
            .checked_sub(page_virtual)
            .ok_or(BuddyError::Unaddressable)?;
        let capacity = (PAGE_SIZE as usize)
            .checked_sub(object_offset)
            .ok_or(BuddyError::Unaddressable)?
            / object_size;
        if capacity == 0 || capacity > u16::MAX as usize {
            allocator_fault(AllocatorFault::InvalidSlabHeader);
        }

        for index in 0..capacity {
            let object =
                core::ptr::with_exposed_provenance_mut::<usize>(object_start + index * object_size);
            let next = if index + 1 == capacity {
                0
            } else {
                object_start + (index + 1) * object_size
            };
            // SAFETY: The newly allocated slab page is writable and exclusive.
            unsafe { object.write(next) };
        }

        // SAFETY: The header is suitably aligned at the start of a page.
        unsafe {
            header_pointer.write(SlabHeader {
                magic: SLAB_MAGIC,
                physical,
                previous_partial: SlabLink::NONE.raw(),
                next_partial: old_head.map_or(SlabLink::NONE, SlabLink::from_page).raw(),
                free_head: object_start,
                in_use: 0,
                capacity: capacity as u16,
                class: class.raw(),
                list_state: SLAB_LINKED,
                pool: pool as u8,
                _reserved: 0,
            })
        };
        if let Some(old_head) = old_head_pointer {
            // SAFETY: The old head was prevalidated and remains stable while
            // this allocator is exclusively borrowed.
            unsafe { (*old_head.as_ptr()).previous_partial = physical };
        }
        permit.commit();
        let _ = pending.publish();
        self.slab_pages = slab_pages;
        #[cfg(feature = "heap-profiling")]
        {
            let backing = &mut self.profile_backing
                [crate::mm::allocator::heap::profile::bucket(pool, Some(class))];
            backing.pages += 1;
            backing.capacity += capacity;
        }
        if pool == HeapPool::FileCache {
            self.file_cache_slab_pages += 1;
        }
        Ok(())
    }

    fn slab_class_for(&self, layout: Layout) -> Option<HeapSlabClass> {
        let class = slab_class_for_layout(layout)?;
        self.partial[HeapPool::Kernel as usize].class(class.index())
    }

    fn allocate_large(&mut self, layout: Layout) -> Result<*mut u8, LargeAllocationError> {
        self.allocate_large_in(layout, HeapPool::Kernel)
    }

    fn allocate_large_in(
        &mut self,
        layout: Layout,
        pool: HeapPool,
    ) -> Result<*mut u8, LargeAllocationError> {
        let header_size = core::mem::size_of::<LargeHeader>();
        let power_of_two_pages =
            large_allocation_pages(layout).ok_or(LargeAllocationError::UnsupportedLayout)?;
        let order = power_of_two_pages.trailing_zeros() as usize;
        let pending = PendingBuddyBlock::allocate(&mut self.buddy, order).map_err(|error| {
            if error == BuddyError::OutOfMemory {
                LargeAllocationError::OutOfMemory
            } else {
                LargeAllocationError::UnsupportedLayout
            }
        })?;
        let physical = pending.physical().get();
        let Some(base) = pending.virtual_address() else {
            return Err(LargeAllocationError::UnsupportedLayout);
        };
        let Some(user) = base
            .checked_add(header_size)
            .and_then(|header_end| align_up_usize(header_end, layout.align()))
        else {
            return Err(LargeAllocationError::UnsupportedLayout);
        };
        let block_size = match power_of_two_pages.checked_mul(PAGE_SIZE as usize) {
            Some(block_size) => block_size,
            None => return Err(LargeAllocationError::UnsupportedLayout),
        };
        let allocation_end = match base.checked_add(block_size) {
            Some(allocation_end) => allocation_end,
            None => return Err(LargeAllocationError::UnsupportedLayout),
        };
        let object_end = match user.checked_add(layout.size().max(1)) {
            Some(object_end) => object_end,
            None => return Err(LargeAllocationError::UnsupportedLayout),
        };
        if object_end > allocation_end {
            return Err(LargeAllocationError::UnsupportedLayout);
        }
        let header = (user - header_size) as *mut LargeHeader;
        // SAFETY: The header lies inside the exclusively owned buddy block.
        unsafe {
            header.write(LargeHeader {
                magic: LARGE_MAGIC,
                physical,
                order: order as u8,
                pool: pool as u8,
                _reserved: [0; 6],
            })
        };
        let _ = pending.publish();
        self.large_heap_pages = match self.large_heap_pages.checked_add(1usize << order) {
            Some(pages) => pages,
            None => allocator_fault(AllocatorFault::AllocationCountOverflow),
        };
        if pool == HeapPool::FileCache {
            self.file_cache_large_pages += 1usize << order;
        }
        #[cfg(feature = "heap-profiling")]
        {
            self.profile_backing[crate::mm::allocator::heap::profile::bucket(pool, None)].pages +=
                1usize << order;
        }
        Ok(user as *mut u8)
    }

    unsafe fn deallocate_large(&mut self, pointer: *mut u8, layout: Layout) {
        let header_size = core::mem::size_of::<LargeHeader>();
        let header_address = match (pointer as usize).checked_sub(header_size) {
            Some(header_address) => header_address,
            None => allocator_fault(AllocatorFault::InvalidLargePointer),
        };
        // SAFETY: GlobalAlloc requires `pointer` to be a live large allocation.
        let header = unsafe { *(header_address as *const LargeHeader) };
        if header.magic != LARGE_MAGIC {
            allocator_fault(AllocatorFault::InvalidLargeHeader);
        }
        let order = usize::from(header.order);
        if order > MAX_ORDER {
            allocator_fault(AllocatorFault::InvalidLargeHeader);
        }
        let pages = 1usize << order;
        let Some(base) = self.virtual_address(header.physical) else {
            allocator_fault(AllocatorFault::InvalidLargePointer);
        };
        let expected = base
            .checked_add(header_size)
            .and_then(|header_end| align_up_usize(header_end, layout.align()));
        let block_size = match pages.checked_mul(PAGE_SIZE as usize) {
            Some(block_size) => block_size,
            None => allocator_fault(AllocatorFault::InvalidLargeHeader),
        };
        let allocation_end = base.checked_add(block_size);
        let object_end = (pointer as usize).checked_add(layout.size().max(1));
        let object_fits = matches!(
            (object_end, allocation_end),
            (Some(object_end), Some(allocation_end)) if object_end <= allocation_end
        );
        if expected != Some(pointer as usize) || !object_fits {
            allocator_fault(AllocatorFault::InvalidLargePointer);
        }
        if self.large_heap_pages < pages {
            allocator_fault(AllocatorFault::AllocationCountUnderflow);
        }
        // SAFETY: The header records the exact buddy allocation.
        if unsafe {
            self.buddy
                .deallocate(PhysicalAddress::new(header.physical), order)
        }
        .is_err()
        {
            allocator_fault(AllocatorFault::BuddyDeallocation);
        }
        self.large_heap_pages -= pages;
        #[cfg(feature = "heap-profiling")]
        {
            self.profile_backing[crate::mm::allocator::heap::profile::bucket(
                HeapPool::from_raw(header.pool),
                None,
            )]
            .pages -= pages;
        }
        if HeapPool::from_raw(header.pool) == HeapPool::FileCache {
            self.file_cache_large_pages -= pages;
        }
    }

    fn virtual_address(&self, physical: u64) -> Option<usize> {
        self.buddy
            .direct_map_base()
            .checked_add(physical)
            .and_then(|address| usize::try_from(address).ok())
    }

    pub fn stats(&self) -> HeapStats {
        let buddy = self.buddy.stats();
        HeapStats {
            buddy,
            free_pages: buddy.free_pages,
            slab_pages: self.slab_pages,
            large_heap_pages: self.large_heap_pages,
            file_cache_slab_pages: self.file_cache_slab_pages,
            file_cache_large_pages: self.file_cache_large_pages,
            live_allocations: self.live_allocations,
            live_slab_allocations: self.live_slab_allocations,
            live_large_allocations: self.live_large_allocations,
            peak_live_allocations: self.peak_live_allocations,
            requested_bytes: self.requested_bytes,
            peak_requested_bytes: self.peak_requested_bytes,
            allocation_requests: self.allocation_requests,
            allocation_failures: self.allocation_failures,
            cache: HeapCacheStats::default(),
            kernel_pages: self.page_owners[PageOwner::Kernel.index()],
            page_table_pages: self.page_owners[PageOwner::PageTable.index()],
            guest_pages: self.page_owners[PageOwner::Guest.index()],
            user_pages: self.page_owners[PageOwner::User.index()],
            guest_loaned_pages: self.guest_loaned_pages,
            file_cache_pages: self.page_owners[PageOwner::FileCache.index()],
        }
    }

    /// Changes the guest-use subset without changing physical allocation ownership.
    /// Callers retain the corresponding User pages until after releasing the count.
    fn retain_guest_pages(&mut self, pages: usize) {
        let Some(total) = self.guest_loaned_pages.checked_add(pages) else {
            allocator_fault(AllocatorFault::AllocationCountOverflow);
        };
        if total > self.page_owners[PageOwner::User.index()].pages {
            allocator_fault(AllocatorFault::PageOwnerUnderflow);
        }
        self.guest_loaned_pages = total;
    }

    fn release_guest_pages(&mut self, pages: usize) {
        let Some(total) = self.guest_loaned_pages.checked_sub(pages) else {
            allocator_fault(AllocatorFault::PageOwnerUnderflow);
        };
        self.guest_loaned_pages = total;
    }

    fn allocate_pages(
        &mut self,
        order: usize,
        owner: PageOwner,
    ) -> Result<PhysicalAddress, BuddyError> {
        let owner_stats = &mut self.page_owners[owner.index()];
        owner_stats.allocation_requests = owner_stats.allocation_requests.saturating_add(1);
        match self.buddy.allocate(order) {
            Ok(address) => {
                let pages = match 1usize.checked_shl(order as u32) {
                    Some(pages) => pages,
                    None => allocator_fault(AllocatorFault::AllocationCountOverflow),
                };
                owner_stats.pages = match owner_stats.pages.checked_add(pages) {
                    Some(pages) => pages,
                    None => allocator_fault(AllocatorFault::AllocationCountOverflow),
                };
                owner_stats.peak_pages = owner_stats.peak_pages.max(owner_stats.pages);
                Ok(address)
            }
            Err(error) => {
                owner_stats.allocation_failures = owner_stats.allocation_failures.saturating_add(1);
                Err(error)
            }
        }
    }

    unsafe fn deallocate_pages(
        &mut self,
        address: PhysicalAddress,
        order: usize,
        owner: PageOwner,
    ) -> Result<(), BuddyError> {
        let pages = 1usize
            .checked_shl(order as u32)
            .ok_or(BuddyError::InvalidOrder)?;
        let owner_stats = &mut self.page_owners[owner.index()];
        if owner_stats.pages < pages {
            allocator_fault(AllocatorFault::PageOwnerUnderflow);
        }
        // SAFETY: The caller guarantees this exact live owner/order allocation;
        // accounting above validates its page quantity before relinquishment.
        unsafe { self.buddy.deallocate(address, order)? };
        owner_stats.pages -= pages;
        owner_stats.deallocations = owner_stats.deallocations.saturating_add(1);
        Ok(())
    }
}

/// Conservative physical backing for one heap allocation, including headers
/// and alignment. Small objects may share an existing slab; charging a whole
/// slab page also covers a CPU-magazine refill for the current class limits.
pub fn allocation_page_bound(layout: Layout) -> Option<usize> {
    if slab_class_for_layout(layout).is_some() {
        Some(1)
    } else {
        large_allocation_pages(layout)
    }
}

fn large_allocation_pages(layout: Layout) -> Option<usize> {
    let required = layout
        .size()
        .max(1)
        .checked_add(layout.align() - 1)?
        .checked_add(core::mem::size_of::<LargeHeader>())?;
    let pages = required
        .div_ceil(PAGE_SIZE as usize)
        .checked_next_power_of_two()?;
    (pages.trailing_zeros() as usize <= MAX_ORDER).then_some(pages)
}

fn class_index_for(layout: Layout) -> Option<usize> {
    let required = layout.size().max(layout.align()).max(1);
    CLASS_SIZES.iter().position(|&size| size >= required)
}

fn slab_class_for_layout(layout: Layout) -> Option<HeapSlabClass> {
    class_index_for(layout).and_then(HeapSlabClass::new)
}

fn align_up_usize(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
}

fn increment(value: &mut usize) {
    *value = match value.checked_add(1) {
        Some(value) => value,
        None => allocator_fault(AllocatorFault::AllocationCountOverflow),
    };
}
