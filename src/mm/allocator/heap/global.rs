// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Synchronized global-allocation adapter with bounded CPU-local magazines.
//!
//! The central heap remains the sole owner of buddy pages, slab headers, and
//! partial-list topology. CPU-local magazines only hold linear tokens for slab
//! objects which remain reserved in that topology. No local slot lock is held
//! while acquiring the central heap lock.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{NonNull, null_mut, write_bytes};

use crate::cpu::{CpuIndex, PerCpu, PinnedExecution};
use crate::hal::interrupt::InterruptMask;
use crate::mm::{BuddyError, MemoryHandoff, PhysicalAddress};
use crate::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use crate::sync::{InterruptMaskGuard, InterruptSpinLock, SpinLock};

use super::local_cache::{MAGAZINE_STORAGE, Magazine, PushError};
use super::{
    AllocatorFault, CachedObject, HeapCacheStats, HeapSlabClass, HeapStats, InitError,
    LargeAllocationError, PageOwner, SlabAllocator, allocator_fault,
};

const CACHED_CLASS_COUNT: usize = 6;
const CACHE_LIMITS: [usize; CACHED_CLASS_COUNT] = [16, 16, 12, 8, 4, 2];

#[derive(Clone, Copy, Eq, PartialEq)]
enum CacheReclaimReason {
    Explicit,
    MemoryPressure,
}

const _: () = {
    assert!(CACHED_CLASS_COUNT <= super::CLASS_SIZES.len());
    let mut class = 0;
    while class < CACHED_CLASS_COUNT {
        assert!(CACHE_LIMITS[class] > 0);
        assert!(CACHE_LIMITS[class] <= MAGAZINE_STORAGE);
        class += 1;
    }
};

/// Kernel policy required to access a CPU-local allocator magazine.
///
/// # Safety
///
/// A successful `pin` must prevent migration until its value is dropped.
/// `current_cpu` must return the executing CPU while that pin is held. The
/// interrupt-mask implementation must compose with the pin and restore the
/// exact prior local state.
pub unsafe trait CpuLocalCachePolicy: InterruptMask {
    type Pin: PinnedExecution;

    fn pin() -> Option<Self::Pin>;
    fn current_cpu(pin: &Self::Pin) -> Option<CpuIndex>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheActivationError {
    AllocatorUnavailable,
    AlreadyEnabled,
    InvalidCpuCount,
}

struct AllocatorState {
    heap: Option<SlabAllocator>,
}

impl AllocatorState {
    const fn uninitialized() -> Self {
        Self { heap: None }
    }
}

struct CpuCache {
    magazines: [Magazine<CachedObject>; CACHED_CLASS_COUNT],
}

impl CpuCache {
    const fn new() -> Self {
        Self {
            magazines: [const { Magazine::new() }; CACHED_CLASS_COUNT],
        }
    }

    fn pop(&mut self, class: HeapSlabClass) -> Option<CachedObject> {
        match self.magazines.get_mut(class.index())?.pop() {
            Ok(object) => object,
            Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
        }
    }

    fn push(&mut self, object: CachedObject) -> Result<(), PushError<CachedObject>> {
        let class = object.class().index();
        let Some(magazine) = self.magazines.get_mut(class) else {
            return Err(PushError::InvalidState(object));
        };
        magazine.push(object, CACHE_LIMITS[class])
    }

    fn detach_drain(&mut self, class: HeapSlabClass) -> Magazine<CachedObject> {
        let index = class.index();
        match self.magazines[index].take(CACHE_LIMITS[index].div_ceil(2)) {
            Ok(batch) => batch,
            Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
        }
    }

    fn detach_all(&mut self, class: usize) -> Magazine<CachedObject> {
        match self.magazines[class].take(MAGAZINE_STORAGE) {
            Ok(batch) => batch,
            Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
        }
    }
}

struct CacheSlot {
    cache: SpinLock<CpuCache>,
}

impl CacheSlot {
    const fn new() -> Self {
        Self {
            cache: SpinLock::new(CpuCache::new()),
        }
    }
}

struct LogicalAccounting {
    live: AtomicUsize,
    live_slab: AtomicUsize,
    live_large: AtomicUsize,
    peak_live: AtomicUsize,
    requested_bytes: AtomicUsize,
    peak_requested_bytes: AtomicUsize,
    requests: AtomicU64,
    failures: AtomicU64,
}

impl LogicalAccounting {
    const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            live_slab: AtomicUsize::new(0),
            live_large: AtomicUsize::new(0),
            peak_live: AtomicUsize::new(0),
            requested_bytes: AtomicUsize::new(0),
            peak_requested_bytes: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        }
    }

    fn record_request(&self) {
        saturating_increment_u64(&self.requests);
    }

    fn record_failure(&self) {
        saturating_increment_u64(&self.failures);
    }

    fn record_allocation(&self, layout: Layout, slab: bool) {
        let live = checked_add(&self.live, 1);
        let requested = checked_add(&self.requested_bytes, layout.size().max(1));
        if slab {
            let _ = checked_add(&self.live_slab, 1);
        } else {
            let _ = checked_add(&self.live_large, 1);
        }
        self.peak_live.fetch_max(live, Ordering::Relaxed);
        self.peak_requested_bytes
            .fetch_max(requested, Ordering::Relaxed);
    }

    fn record_deallocation(&self, layout: Layout, slab: bool) {
        let _ = checked_sub(&self.live, 1);
        let _ = checked_sub(&self.requested_bytes, layout.size().max(1));
        if slab {
            let _ = checked_sub(&self.live_slab, 1);
        } else {
            let _ = checked_sub(&self.live_large, 1);
        }
    }

    fn apply(&self, stats: &mut HeapStats) {
        stats.live_allocations = self.live.load(Ordering::Relaxed);
        stats.live_slab_allocations = self.live_slab.load(Ordering::Relaxed);
        stats.live_large_allocations = self.live_large.load(Ordering::Relaxed);
        stats.peak_live_allocations = self.peak_live.load(Ordering::Relaxed);
        stats.requested_bytes = self.requested_bytes.load(Ordering::Relaxed);
        stats.peak_requested_bytes = self.peak_requested_bytes.load(Ordering::Relaxed);
        stats.allocation_requests = self.requests.load(Ordering::Relaxed);
        stats.allocation_failures = self.failures.load(Ordering::Relaxed);
    }
}

struct CacheAccounting {
    cached_objects: AtomicUsize,
    hits: AtomicU64,
    misses: AtomicU64,
    refills: AtomicU64,
    drains: AtomicU64,
    pressure_reclaims: AtomicU64,
    reclaimed_objects: AtomicU64,
}

impl CacheAccounting {
    const fn new() -> Self {
        Self {
            cached_objects: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            refills: AtomicU64::new(0),
            drains: AtomicU64::new(0),
            pressure_reclaims: AtomicU64::new(0),
            reclaimed_objects: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, enabled_cpus: usize) -> HeapCacheStats {
        HeapCacheStats {
            enabled_cpus,
            cached_objects: self.cached_objects.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            refills: self.refills.load(Ordering::Relaxed),
            drains: self.drains.load(Ordering::Relaxed),
            pressure_reclaims: self.pressure_reclaims.load(Ordering::Relaxed),
            reclaimed_objects: self.reclaimed_objects.load(Ordering::Relaxed),
        }
    }
}

pub struct KernelGlobalAllocator<P: CpuLocalCachePolicy> {
    state: InterruptSpinLock<AllocatorState, P>,
    caches: PerCpu<CacheSlot>,
    initialized: AtomicBool,
    caches_enabled: AtomicBool,
    participating_cpus: AtomicUsize,
    logical: LogicalAccounting,
    cache_accounting: CacheAccounting,
}

impl<P: CpuLocalCachePolicy> KernelGlobalAllocator<P> {
    pub const fn new() -> Self {
        Self {
            state: InterruptSpinLock::new(AllocatorState::uninitialized()),
            caches: PerCpu::new([const { CacheSlot::new() }; crate::cpu::MAX_CPUS]),
            initialized: AtomicBool::new(false),
            caches_enabled: AtomicBool::new(false),
            participating_cpus: AtomicUsize::new(0),
            logical: LogicalAccounting::new(),
            cache_accounting: CacheAccounting::new(),
        }
    }

    /// Installs the physical-page handoff as Rust's global heap.
    ///
    /// # Safety
    ///
    /// `direct_map_base` must permanently map all handed-off RAM as writable
    /// Normal memory and preserve page alignment. Initialization must occur
    /// exactly once before allocation.
    pub unsafe fn initialize(
        &self,
        handoff: &MemoryHandoff,
        direct_map_base: u64,
    ) -> Result<(), InitError> {
        self.state.with(|state| {
            if state.heap.is_some() {
                return Err(InitError::AlreadyInitialized);
            }
            // SAFETY: The public initialize contract guarantees the permanent
            // direct map, and the lock plus Option enforces one-time creation.
            state.heap = Some(unsafe { SlabAllocator::from_handoff(handoff, direct_map_base)? });
            Ok(())
        })?;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Enables preinitialized empty magazines for the frozen CPU topology.
    pub fn activate_local_caches(
        &self,
        participating_cpus: usize,
    ) -> Result<(), CacheActivationError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(CacheActivationError::AllocatorUnavailable);
        }
        if participating_cpus == 0 || participating_cpus > crate::cpu::MAX_CPUS {
            return Err(CacheActivationError::InvalidCpuCount);
        }
        self.participating_cpus
            .compare_exchange(0, participating_cpus, Ordering::Relaxed, Ordering::Relaxed)
            .map_err(|_| CacheActivationError::AlreadyEnabled)?;
        self.caches_enabled.store(true, Ordering::Release);
        Ok(())
    }

    pub fn stats(&self) -> Option<HeapStats> {
        let mut stats = self
            .state
            .with(|state| state.heap.as_ref().map(SlabAllocator::stats))?;
        self.logical.apply(&mut stats);
        let enabled_cpus = if self.caches_enabled.load(Ordering::Acquire) {
            self.participating_cpus.load(Ordering::Relaxed)
        } else {
            0
        };
        stats.cache = self.cache_accounting.snapshot(enabled_cpus);
        Some(stats)
    }

    pub fn allocate_pages(&self, order: usize) -> Result<PhysicalAddress, BuddyError> {
        self.allocate_pages_for(order, PageOwner::Kernel)
    }

    pub fn allocate_pages_for(
        &self,
        order: usize,
        owner: PageOwner,
    ) -> Result<PhysicalAddress, BuddyError> {
        let first = self.state.with(|state| {
            let heap = state.heap.as_mut().ok_or(BuddyError::OutOfMemory)?;
            heap.allocate_pages(order, owner)
        });
        if first != Err(BuddyError::OutOfMemory) || !self.caches_enabled.load(Ordering::Acquire) {
            return first;
        }
        let _ = self.reclaim_local_caches_internal(CacheReclaimReason::MemoryPressure);
        self.state.with(|state| {
            let heap = state.heap.as_mut().ok_or(BuddyError::OutOfMemory)?;
            heap.allocate_pages(order, owner)
        })
    }

    /// # Safety
    ///
    /// `address` and `order` must identify one inactive live block issued by
    /// this allocator.
    pub unsafe fn deallocate_pages(
        &self,
        address: PhysicalAddress,
        order: usize,
    ) -> Result<(), BuddyError> {
        // SAFETY: The exact live block contract is forwarded unchanged.
        unsafe { self.deallocate_pages_for(address, order, PageOwner::Kernel) }
    }

    /// # Safety
    ///
    /// The address, order, and owner must exactly match a live allocation.
    pub unsafe fn deallocate_pages_for(
        &self,
        address: PhysicalAddress,
        order: usize,
        owner: PageOwner,
    ) -> Result<(), BuddyError> {
        self.state.with(|state| {
            let heap = state.heap.as_mut().ok_or(BuddyError::OutOfMemory)?;
            // SAFETY: The public contract supplies the exact block and owner.
            unsafe { heap.deallocate_pages(address, order, owner) }
        })
    }

    /// Best-effort reclaim of objects currently visible in CPU magazines.
    ///
    /// Concurrent allocations may refill a slot already visited by this pass;
    /// this method is not a cache-disable or teardown barrier.
    pub fn reclaim_local_caches(&self) -> usize {
        self.reclaim_local_caches_internal(CacheReclaimReason::Explicit)
    }

    fn pin_current_cache(&self) -> (P::Pin, CpuIndex) {
        let pin = match P::pin() {
            Some(pin) => pin,
            None => allocator_fault(AllocatorFault::CachePolicyFailure),
        };
        let Some(cpu) = P::current_cpu(&pin) else {
            allocator_fault(AllocatorFault::CachePolicyFailure);
        };
        if cpu.get() >= self.participating_cpus.load(Ordering::Relaxed) {
            allocator_fault(AllocatorFault::CachePolicyFailure);
        }
        (pin, cpu)
    }

    fn allocate_cached(&self, class: HeapSlabClass) -> *mut u8 {
        for attempt in 0..2 {
            let (_pin, cpu) = self.pin_current_cache();
            // SAFETY: The pin fixes `cpu`; masking excludes a same-CPU IRQ
            // allocator entry while the selected local slot is locked.
            let mask = unsafe { InterruptMaskGuard::<P>::acquire() };
            let cached = self.caches[cpu].cache.with(|cache| cache.pop(class));
            drop(mask);
            drop(_pin);
            if let Some(object) = cached {
                checked_sub(&self.cache_accounting.cached_objects, 1);
                saturating_increment_u64(&self.cache_accounting.hits);
                return object.into_caller_pointer();
            }
            saturating_increment_u64(&self.cache_accounting.misses);

            let mut batch = Magazine::new();
            let refill = CACHE_LIMITS[class.index()].div_ceil(2);
            self.state.with(|state| {
                let Some(heap) = state.heap.as_mut() else {
                    return;
                };
                for _ in 0..refill {
                    let Some(object) = heap.reserve_slab_object(class) else {
                        break;
                    };
                    match batch.push(object, MAGAZINE_STORAGE) {
                        Ok(()) => {}
                        Err(PushError::Full(object)) => {
                            heap.release_slab_object(object);
                            break;
                        }
                        Err(PushError::InvalidState(_)) => {
                            allocator_fault(AllocatorFault::InvalidCacheState)
                        }
                    }
                }
            });

            let caller = match batch.pop() {
                Ok(Some(caller)) => caller,
                Ok(None) => {
                    if attempt == 0 {
                        let _ =
                            self.reclaim_local_caches_internal(CacheReclaimReason::MemoryPressure);
                        continue;
                    }
                    return null_mut();
                }
                Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
            };
            saturating_increment_u64(&self.cache_accounting.refills);
            let mut rejected = Magazine::new();
            let (_pin, cpu) = self.pin_current_cache();
            // SAFETY: The new pin fixes `cpu` while masking excludes same-CPU
            // IRQ entry during this slot mutation. A refill may migrate before
            // this point; unused objects simply join the destination cache.
            let mask = unsafe { InterruptMaskGuard::<P>::acquire() };
            self.caches[cpu].cache.with(|cache| {
                loop {
                    let object = match batch.pop() {
                        Ok(Some(object)) => object,
                        Ok(None) => break,
                        Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
                    };
                    match cache.push(object) {
                        Ok(()) => {
                            let _ = checked_add(&self.cache_accounting.cached_objects, 1);
                        }
                        Err(PushError::Full(object)) => {
                            match rejected.push(object, MAGAZINE_STORAGE) {
                                Ok(()) => {}
                                Err(PushError::Full(_)) | Err(PushError::InvalidState(_)) => {
                                    allocator_fault(AllocatorFault::InvalidCacheState)
                                }
                            }
                        }
                        Err(PushError::InvalidState(_)) => {
                            allocator_fault(AllocatorFault::InvalidCacheState)
                        }
                    }
                }
            });
            drop(mask);
            drop(_pin);
            self.release_batch(rejected);
            return caller.into_caller_pointer();
        }
        null_mut()
    }

    fn allocate_central(&self, layout: Layout, class: Option<HeapSlabClass>) -> *mut u8 {
        let allocate = || {
            self.state.with(|state| {
                let Some(heap) = state.heap.as_mut() else {
                    return Err(LargeAllocationError::UnsupportedLayout);
                };
                match class {
                    Some(class) => heap
                        .reserve_slab_object(class)
                        .map(CachedObject::into_caller_pointer)
                        .ok_or(LargeAllocationError::OutOfMemory),
                    None => heap.allocate_large(layout),
                }
            })
        };
        let first = allocate();
        match first {
            Ok(pointer) => return pointer,
            Err(LargeAllocationError::UnsupportedLayout) => return null_mut(),
            Err(LargeAllocationError::OutOfMemory)
                if !self.caches_enabled.load(Ordering::Acquire) =>
            {
                return null_mut();
            }
            Err(LargeAllocationError::OutOfMemory) => {}
        }
        let _ = self.reclaim_local_caches_internal(CacheReclaimReason::MemoryPressure);
        allocate().unwrap_or(null_mut())
    }

    unsafe fn deallocate_cached(&self, pointer: NonNull<u8>, class: HeapSlabClass) {
        // SAFETY: GlobalAlloc supplies the exact live object and layout class.
        let object = unsafe { CachedObject::from_caller(pointer, class) };
        let (_pin, cpu) = self.pin_current_cache();
        let mut drain = Magazine::new();
        let mut returned = Some(object);
        // SAFETY: The pin fixes `cpu`; masking excludes a same-CPU IRQ
        // allocator entry while the selected local slot is locked.
        let mask = unsafe { InterruptMaskGuard::<P>::acquire() };
        self.caches[cpu].cache.with(|cache| {
            let object = match returned.take() {
                Some(object) => object,
                None => allocator_fault(AllocatorFault::InvalidCacheState),
            };
            match cache.push(object) {
                Ok(()) => {
                    let _ = checked_add(&self.cache_accounting.cached_objects, 1);
                }
                Err(PushError::Full(object)) => {
                    drain = cache.detach_drain(class);
                    let drained = drain.len();
                    if drained != 0 {
                        checked_sub(&self.cache_accounting.cached_objects, drained);
                    }
                    match cache.push(object) {
                        Ok(()) => {
                            let _ = checked_add(&self.cache_accounting.cached_objects, 1);
                        }
                        Err(PushError::Full(object)) => returned = Some(object),
                        Err(PushError::InvalidState(_)) => {
                            allocator_fault(AllocatorFault::InvalidCacheState)
                        }
                    }
                }
                Err(PushError::InvalidState(_)) => {
                    allocator_fault(AllocatorFault::InvalidCacheState)
                }
            }
        });
        drop(mask);
        drop(_pin);
        if let Some(object) = returned {
            match drain.push(object, MAGAZINE_STORAGE) {
                Ok(()) => {}
                Err(PushError::Full(_)) | Err(PushError::InvalidState(_)) => {
                    allocator_fault(AllocatorFault::InvalidCacheState)
                }
            }
        }
        if !drain.is_empty() {
            saturating_increment_u64(&self.cache_accounting.drains);
        }
        self.release_batch(drain);
    }

    unsafe fn deallocate_central(
        &self,
        pointer: NonNull<u8>,
        layout: Layout,
        class: Option<HeapSlabClass>,
    ) {
        self.state.with(|state| {
            let Some(heap) = state.heap.as_mut() else {
                allocator_fault(AllocatorFault::UninitializedDeallocation);
            };
            match class {
                Some(class) => {
                    // SAFETY: GlobalAlloc supplies the exact live object and class.
                    let object = unsafe { CachedObject::from_caller(pointer, class) };
                    heap.release_slab_object(object);
                }
                None => {
                    // SAFETY: GlobalAlloc supplies the exact live large allocation.
                    unsafe { heap.deallocate_large(pointer.as_ptr(), layout) };
                }
            }
        });
    }

    fn release_batch(&self, mut batch: Magazine<CachedObject>) -> usize {
        let count = batch.len();
        if count == 0 {
            return 0;
        }
        self.state.with(|state| {
            let Some(heap) = state.heap.as_mut() else {
                allocator_fault(AllocatorFault::InvalidCacheState);
            };
            loop {
                match batch.pop() {
                    Ok(Some(object)) => heap.release_slab_object(object),
                    Ok(None) => break,
                    Err(_) => allocator_fault(AllocatorFault::InvalidCacheState),
                }
            }
        });
        count
    }

    fn reclaim_local_caches_internal(&self, reason: CacheReclaimReason) -> usize {
        if !self.caches_enabled.load(Ordering::Acquire) {
            return 0;
        }
        let participants = self.participating_cpus.load(Ordering::Relaxed);
        let mut reclaimed = 0usize;
        for index in 0..participants {
            let Some(cpu) = CpuIndex::new(index) else {
                allocator_fault(AllocatorFault::InvalidCacheState);
            };
            for class in 0..CACHED_CLASS_COUNT {
                let pin = match P::pin() {
                    Some(pin) => pin,
                    None => allocator_fault(AllocatorFault::CachePolicyFailure),
                };
                // SAFETY: Pinning fixes this execution context. Masking only
                // spans slot detachment, excluding same-CPU IRQ re-entry
                // without extending across central slab operations.
                let mask = unsafe { InterruptMaskGuard::<P>::acquire() };
                let batch = self.caches[cpu].cache.with(|cache| cache.detach_all(class));
                drop(mask);
                drop(pin);
                let count = batch.len();
                if count != 0 {
                    checked_sub(&self.cache_accounting.cached_objects, count);
                    reclaimed = match reclaimed.checked_add(self.release_batch(batch)) {
                        Some(reclaimed) => reclaimed,
                        None => allocator_fault(AllocatorFault::CacheAccountingOverflow),
                    };
                }
            }
        }
        if reclaimed != 0 {
            if reason == CacheReclaimReason::MemoryPressure {
                saturating_increment_u64(&self.cache_accounting.pressure_reclaims);
            }
            saturating_add_u64(&self.cache_accounting.reclaimed_objects, reclaimed as u64);
        }
        reclaimed
    }
}

impl<P: CpuLocalCachePolicy> Default for KernelGlobalAllocator<P> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: The central lock serializes all slab/buddy metadata. CPU-local
// magazine access requires a policy pin, local IRQ masking, and a per-slot
// lock. Linear CachedObject values preserve exclusive ownership across every
// transfer and keep their central slab pages resident.
unsafe impl<P: CpuLocalCachePolicy> GlobalAlloc for KernelGlobalAllocator<P> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.initialized.load(Ordering::Acquire) {
            return null_mut();
        }
        self.logical.record_request();
        let class = super::slab_class_for_layout(layout);
        let pointer = match class {
            Some(class)
                if class.index() < CACHED_CLASS_COUNT
                    && self.caches_enabled.load(Ordering::Acquire) =>
            {
                self.allocate_cached(class)
            }
            _ => self.allocate_central(layout, class),
        };
        if pointer.is_null() {
            self.logical.record_failure();
        } else {
            self.logical.record_allocation(layout, class.is_some());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc forwards the valid requested layout to alloc.
        let pointer = unsafe { self.alloc(layout) };
        if !pointer.is_null() {
            // SAFETY: alloc returned at least layout.size() writable exclusive
            // bytes for this exact layout.
            unsafe { write_bytes(pointer, 0, layout.size()) };
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if !self.initialized.load(Ordering::Acquire) {
            allocator_fault(AllocatorFault::UninitializedDeallocation);
        }
        let Some(pointer) = NonNull::new(pointer) else {
            allocator_fault(AllocatorFault::InvalidSlabPointer);
        };
        let class = super::slab_class_for_layout(layout);
        self.logical.record_deallocation(layout, class.is_some());
        match class {
            Some(class)
                if class.index() < CACHED_CLASS_COUNT
                    && self.caches_enabled.load(Ordering::Acquire) =>
            {
                // SAFETY: GlobalAlloc requires the exact live pointer/layout.
                unsafe { self.deallocate_cached(pointer, class) };
            }
            _ => {
                // SAFETY: GlobalAlloc requires the exact live pointer/layout.
                unsafe { self.deallocate_central(pointer, layout, class) };
            }
        }
    }
}

fn checked_add(value: &AtomicUsize, amount: usize) -> usize {
    match value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(amount)
    }) {
        Ok(previous) => previous + amount,
        Err(_) => allocator_fault(AllocatorFault::CacheAccountingOverflow),
    }
}

fn checked_sub(value: &AtomicUsize, amount: usize) -> usize {
    match value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_sub(amount)
    }) {
        Ok(previous) => previous - amount,
        Err(_) => allocator_fault(AllocatorFault::CacheAccountingUnderflow),
    }
}

fn saturating_increment_u64(value: &AtomicU64) {
    saturating_add_u64(value, 1);
}

fn saturating_add_u64(value: &AtomicU64, amount: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}
