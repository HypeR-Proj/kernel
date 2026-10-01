// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Buddy, slab, and owner-accounted runtime allocation contracts.

use std::alloc::{GlobalAlloc, Layout, alloc_zeroed, dealloc};
use std::cell::Cell;
use std::collections::HashSet;
use std::mem::ManuallyDrop;

use hyper::hal::interrupt::InterruptMask;
use hyper::mm::allocator::heap::{
    AllocatorInvariant, AllocatorInvariantInstallError, AllocatorInvariantReport,
    CacheActivationError, CpuLocalCachePolicy, InitError, KernelGlobalAllocator, PageAvailability,
    PageOwner, SlabAllocator, allocation_page_bound, install_allocator_invariant_handler,
};
use hyper::mm::{BootAllocator, BuddyAllocator, BuddyError, PAGE_SIZE};
use hyper::platform::{MAX_MEMORY_REGIONS, MAX_RESERVED_REGIONS, PhysicalRange, RegionList};

struct AlignedMemory {
    pointer: *mut u8,
    layout: Layout,
}

impl AlignedMemory {
    fn new(pages: usize) -> Self {
        let layout = crate::require_ok(Layout::from_size_align(
            pages * PAGE_SIZE as usize,
            PAGE_SIZE as usize,
        ));
        // SAFETY: The test owns the allocation until `Drop`.
        let pointer = unsafe { alloc_zeroed(layout) };
        assert!(!pointer.is_null());
        Self { pointer, layout }
    }
}

impl Drop for AlignedMemory {
    fn drop(&mut self) {
        // SAFETY: `pointer` was allocated with this exact layout.
        unsafe { dealloc(self.pointer, self.layout) };
    }
}

fn handoff(pages: usize) -> (AlignedMemory, hyper::mm::MemoryHandoff) {
    let memory_buffer = AlignedMemory::new(pages);
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(
        0,
        pages as u64 * PAGE_SIZE,
    ))));
    let reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    let boot = crate::require_ok(BootAllocator::new(
        &memory,
        &reserved,
        pages as u64 * PAGE_SIZE,
    ));
    (memory_buffer, boot.handoff())
}

#[test]
fn buddy_splits_and_coalesces_blocks() {
    let (memory, handoff) = handoff(64);
    // SAFETY: The aligned test buffer is the direct map for physical zero.
    let mut buddy =
        crate::require_ok(unsafe { BuddyAllocator::from_handoff(&handoff, memory.pointer as u64) });
    let initial = buddy.free_pages();
    let initial_stats = buddy.stats();
    assert_eq!(initial_stats.managed_pages, 64);
    assert_eq!(initial_stats.free_blocks[6], 1);
    let first = crate::require_ok(buddy.allocate(0));
    let second = crate::require_ok(buddy.allocate(2));
    assert_eq!(buddy.free_pages(), initial - 5);
    let allocated = buddy.stats();
    assert_eq!(allocated.allocated_pages, 5);
    assert_eq!(allocated.peak_allocated_pages, 5);
    assert_eq!(allocated.allocation_requests, 2);
    assert_eq!(
        free_pages_from_blocks(&allocated.free_blocks),
        allocated.free_pages
    );

    // SAFETY: Both blocks are live allocations with matching orders.
    unsafe {
        crate::require_ok(buddy.deallocate(first, 0));
        crate::require_ok(buddy.deallocate(second, 2));
    }
    assert_eq!(buddy.free_pages(), initial);
    assert_eq!(buddy.stats().deallocations, 2);
    assert!(buddy.allocate(6).is_ok());
}

#[test]
fn slab_reuses_small_objects_and_returns_empty_pages() {
    let (memory, handoff) = handoff(128);
    // SAFETY: The aligned test buffer is a stable writable direct map.
    let mut slab =
        crate::require_ok(unsafe { SlabAllocator::from_handoff(&handoff, memory.pointer as u64) });
    let initial = slab.stats().free_pages;
    let small = crate::require_ok(Layout::from_size_align(24, 16));
    let large = crate::require_ok(Layout::from_size_align(9000, 4096));

    // SAFETY: Allocations are paired with matching deallocations below.
    unsafe {
        let mut small_objects = Vec::new();
        for _ in 0..200 {
            let pointer = slab.allocate(small);
            assert!(!pointer.is_null());
            small_objects.push(pointer);
        }
        let big = slab.allocate(large);
        assert!(!big.is_null());
        assert_eq!((small_objects[0] as usize) & 15, 0);
        assert_eq!((big as usize) & 4095, 0);
        assert_ne!(small_objects[0], small_objects[1]);
        for pointer in small_objects.into_iter().rev() {
            slab.deallocate(pointer, small);
        }
        slab.deallocate(big, large);
    }

    let stats = slab.stats();
    assert_eq!(stats.live_allocations, 0);
    assert_eq!(stats.slab_pages, 0);
    assert_eq!(stats.large_heap_pages, 0);
    assert_eq!(stats.requested_bytes, 0);
    assert_eq!(stats.peak_live_allocations, 201);
    assert!(stats.peak_requested_bytes >= 200 * 24 + 9000);
    assert_eq!(stats.free_pages, initial);
}

#[test]
fn slab_header_growth_preserves_every_current_class_capacity() {
    const EXPECTED_CAPACITIES: [(usize, usize); 8] = [
        (16, 253),
        (32, 126),
        (64, 63),
        (128, 31),
        (256, 15),
        (512, 7),
        (1024, 3),
        (2048, 1),
    ];

    for (size, expected_capacity) in EXPECTED_CAPACITIES {
        let (memory, handoff) = handoff(128);
        // SAFETY: The aligned test buffer is a stable writable direct map.
        let mut slab = crate::require_ok(unsafe {
            SlabAllocator::from_handoff(&handoff, memory.pointer as u64)
        });
        let layout = crate::require_ok(Layout::from_size_align(size, size));
        let mut pointers = Vec::new();
        for _ in 0..=expected_capacity {
            let pointer = slab.allocate(layout);
            assert!(!pointer.is_null());
            pointers.push(pointer);
        }
        let first_page = pointers[0] as usize & !(PAGE_SIZE as usize - 1);
        assert_eq!(
            pointers
                .iter()
                .take_while(|pointer| {
                    (**pointer as usize & !(PAGE_SIZE as usize - 1)) == first_page
                })
                .count(),
            expected_capacity
        );

        // SAFETY: Every pointer is live and paired with its exact layout.
        unsafe {
            for pointer in pointers {
                slab.deallocate(pointer, layout);
            }
        }
        assert_eq!(slab.stats().slab_pages, 0);
        assert_eq!(slab.stats().buddy.allocated_pages, 0);
    }
}

#[test]
fn empties_a_non_head_partial_slab_without_losing_neighbors() {
    const CAPACITY: usize = 63;

    let (memory, handoff) = handoff(128);
    // SAFETY: The aligned test buffer is a stable writable direct map.
    let mut slab =
        crate::require_ok(unsafe { SlabAllocator::from_handoff(&handoff, memory.pointer as u64) });
    let initial = slab.stats().free_pages;
    let layout = crate::require_ok(Layout::from_size_align(64, 64));
    let mut objects = Vec::new();
    for _ in 0..(CAPACITY * 2 + 5) {
        let pointer = slab.allocate(layout);
        assert!(!pointer.is_null());
        objects.push(pointer);
    }
    assert_eq!(slab.stats().slab_pages, 3);

    // Freeing from the first full slab inserts it ahead of the third partial
    // slab. Emptying the third slab therefore exercises a non-head unlink.
    // SAFETY: These are distinct live objects with the exact allocation layout.
    unsafe {
        slab.deallocate(objects[0], layout);
        for &pointer in &objects[CAPACITY * 2..] {
            slab.deallocate(pointer, layout);
        }
    }
    assert_eq!(slab.stats().slab_pages, 2);

    let reused = slab.allocate(layout);
    assert_eq!(reused, objects[0]);
    assert_eq!(slab.stats().slab_pages, 2);

    // SAFETY: The first two slabs remain live; objects[0] was reallocated above.
    unsafe {
        for &pointer in &objects[..CAPACITY * 2] {
            slab.deallocate(pointer, layout);
        }
    }
    let stats = slab.stats();
    assert_eq!(stats.live_allocations, 0);
    assert_eq!(stats.slab_pages, 0);
    assert_eq!(stats.free_pages, initial);
    assert_eq!(stats.buddy.allocated_pages, 0);
}

#[test]
fn oversized_heap_request_is_rejected_without_consuming_pages() {
    let (memory, handoff) = handoff(64);
    // SAFETY: The aligned test buffer is a stable writable direct map.
    let mut heap =
        crate::require_ok(unsafe { SlabAllocator::from_handoff(&handoff, memory.pointer as u64) });
    let before = heap.stats();
    let layout = crate::require_ok(Layout::from_size_align(
        (PAGE_SIZE as usize) << 19,
        PAGE_SIZE as usize,
    ));

    assert!(heap.allocate(layout).is_null());
    let after = heap.stats();
    assert_eq!(after.free_pages, before.free_pages);
    assert_eq!(after.buddy.allocated_pages, before.buddy.allocated_pages);
    assert_eq!(after.live_allocations, 0);
    assert_eq!(after.allocation_failures, before.allocation_failures + 1);
}

struct TestInterruptMask;

struct TestPin {
    cpu: usize,
}

std::thread_local! {
    static TEST_CPU: Cell<usize> = const { Cell::new(0) };
    static TEST_PIN_DEPTH: Cell<usize> = const { Cell::new(0) };
    static TEST_IRQ_MASKED: Cell<bool> = const { Cell::new(false) };
}

fn select_test_cpu(index: usize) {
    TEST_PIN_DEPTH.with(|depth| assert_eq!(depth.get(), 0));
    TEST_CPU.with(|current| current.set(index));
}

fn set_test_irq_masked(masked: bool) {
    TEST_IRQ_MASKED.with(|current| current.set(masked));
}

fn test_irq_masked() -> bool {
    TEST_IRQ_MASKED.with(Cell::get)
}

// SAFETY: Host allocator tests never migrate their synchronous continuation
// while a TestPin borrow is live.
unsafe impl hyper::cpu::PinnedExecution for TestPin {}

impl Drop for TestPin {
    fn drop(&mut self) {
        TEST_CPU.with(|current| assert_eq!(current.get(), self.cpu));
        TEST_PIN_DEPTH.with(|depth| {
            let current = depth.get();
            assert!(current != 0);
            depth.set(current - 1);
        });
    }
}

impl InterruptMask for TestInterruptMask {
    type State = bool;

    fn save_and_disable() -> Self::State {
        TEST_IRQ_MASKED.with(|masked| masked.replace(true))
    }

    fn restore(state: Self::State) {
        TEST_IRQ_MASKED.with(|masked| masked.set(state));
    }
}

// SAFETY: Tests use one synchronous boot-CPU execution context, and the test
// interrupt mask has exact lexical nesting semantics.
unsafe impl CpuLocalCachePolicy for TestInterruptMask {
    type Pin = TestPin;

    fn pin() -> Option<Self::Pin> {
        let cpu = TEST_CPU.with(Cell::get);
        TEST_PIN_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Some(TestPin { cpu })
    }

    fn current_cpu(pin: &Self::Pin) -> Option<hyper::cpu::CpuIndex> {
        TEST_CPU.with(|current| {
            (current.get() == pin.cpu)
                .then(|| hyper::cpu::CpuIndex::new(current.get()))
                .flatten()
        })
    }
}

#[test]
fn accounts_direct_pages_by_owner() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is the direct map and outlives the allocator.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });

    let guest = crate::require_ok(allocator.allocate_pages_for(3, PageOwner::Guest));
    let table = crate::require_ok(allocator.allocate_pages_for(0, PageOwner::PageTable));
    let user = crate::require_ok(allocator.allocate_pages_for(1, PageOwner::User));
    let cache =
        crate::require_ok(allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, 4));
    let stats = crate::require_some(allocator.stats());
    assert_eq!(stats.guest_pages.pages, 8);
    assert_eq!(stats.page_table_pages.pages, 1);
    assert_eq!(stats.user_pages.pages, 2);
    assert_eq!(stats.file_cache_pages.pages, 1);
    assert_eq!(stats.buddy.allocated_pages, 12);

    // SAFETY: These are the exact live blocks and owners returned above.
    unsafe {
        crate::require_ok(allocator.deallocate_pages_for(table, 0, PageOwner::PageTable));
        crate::require_ok(allocator.deallocate_pages_for(user, 1, PageOwner::User));
        crate::require_ok(allocator.deallocate_pages_for(guest, 3, PageOwner::Guest));
        crate::require_ok(allocator.deallocate_pages_for(cache, 0, PageOwner::FileCache));
    }
    let stats = crate::require_some(allocator.stats());
    assert_eq!(stats.guest_pages.pages, 0);
    assert_eq!(stats.guest_pages.peak_pages, 8);
    assert_eq!(stats.page_table_pages.pages, 0);
    assert_eq!(stats.user_pages.pages, 0);
    assert_eq!(stats.user_pages.peak_pages, 2);
    assert_eq!(stats.file_cache_pages.pages, 0);
    assert_eq!(stats.file_cache_pages.peak_pages, 1);
    assert_eq!(stats.buddy.allocated_pages, 0);
}

#[test]
fn rejects_misaligned_direct_map_without_publishing_heap_state() {
    let (memory, handoff) = handoff(64);
    let allocator = KernelGlobalAllocator::<TestInterruptMask>::new();

    // SAFETY: The constructor rejects the misaligned base before deriving or
    // dereferencing any direct-map pointer from it.
    let error = unsafe { allocator.initialize(&handoff, memory.pointer as u64 + 1) };
    assert_eq!(error, Err(InitError::Buddy(BuddyError::Unaddressable)));
    assert!(allocator.stats().is_none());

    // SAFETY: The aligned test buffer is a stable writable direct map and the
    // failed attempt above did not publish allocator state.
    let initialized = unsafe { allocator.initialize(&handoff, memory.pointer as u64) };
    assert_eq!(initialized, Ok(()));
    assert!(allocator.stats().is_some());

    // SAFETY: This deliberately verifies that one-time publication rejects a
    // second initializer without replacing the live heap.
    let duplicate = unsafe { allocator.initialize(&handoff, memory.pointer as u64) };
    assert_eq!(duplicate, Err(InitError::AlreadyInitialized));
}

#[test]
fn global_adapter_zeroes_reused_storage_and_updates_accounting() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives every allocation made below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(1));
    let layout = crate::require_ok(Layout::from_size_align(96, 32));

    set_test_irq_masked(true);
    // SAFETY: `layout` is valid and the returned allocation is handled using
    // this same allocator and layout.
    let dirty = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert!(test_irq_masked());
    assert!(!dirty.is_null());
    // SAFETY: `dirty` names 96 exclusive writable bytes.
    unsafe { dirty.write_bytes(0xa5, layout.size()) };
    // SAFETY: `dirty` is the live allocation obtained above with this layout.
    unsafe { GlobalAlloc::dealloc(&*allocator, dirty, layout) };
    assert!(test_irq_masked());
    set_test_irq_masked(false);

    // SAFETY: `layout` is valid and the returned allocation is released below.
    let zeroed = unsafe { GlobalAlloc::alloc_zeroed(&*allocator, layout) };
    assert!(!zeroed.is_null());
    // SAFETY: `zeroed` names `layout.size()` initialized bytes until dealloc.
    let bytes = unsafe { std::slice::from_raw_parts(zeroed, layout.size()) };
    assert!(bytes.iter().all(|&byte| byte == 0));
    // SAFETY: `zeroed` is the live allocation obtained above with this layout.
    unsafe { GlobalAlloc::dealloc(&*allocator, zeroed, layout) };

    let cached = crate::require_some(allocator.stats());
    assert_eq!(cached.cache.hits, 1);
    assert!(cached.cache.cached_objects > 0);
    assert!(allocator.reclaim_local_caches() > 0);
    let stats = crate::require_some(allocator.stats());
    assert_eq!(stats.live_allocations, 0);
    assert_eq!(stats.requested_bytes, 0);
    assert_eq!(stats.slab_pages, 0);
}

#[test]
fn local_cache_activation_is_one_way_and_requires_a_valid_topology() {
    let (memory, handoff) = handoff(64);
    let allocator = KernelGlobalAllocator::<TestInterruptMask>::new();
    assert_eq!(
        allocator.activate_local_caches(1),
        Err(CacheActivationError::AllocatorUnavailable)
    );
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the initialized allocator.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    assert_eq!(
        allocator.activate_local_caches(0),
        Err(CacheActivationError::InvalidCpuCount)
    );
    assert_eq!(
        allocator.activate_local_caches(hyper::cpu::MAX_CPUS + 1),
        Err(CacheActivationError::InvalidCpuCount)
    );
    assert_eq!(allocator.activate_local_caches(1), Ok(()));
    assert_eq!(
        allocator.activate_local_caches(1),
        Err(CacheActivationError::AlreadyEnabled)
    );
}

#[test]
fn pre_activation_allocation_survives_cache_activation() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let layout = crate::require_ok(Layout::from_size_align(64, 64));

    // SAFETY: The valid layout is paired with exact deallocation below.
    let pointer = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert!(!pointer.is_null());
    assert_eq!(crate::require_some(allocator.stats()).cache.enabled_cpus, 0);
    crate::require_ok(allocator.activate_local_caches(2));
    select_test_cpu(1);
    // SAFETY: The object remains live across activation and is relinquished
    // with its exact original layout.
    unsafe { GlobalAlloc::dealloc(&*allocator, pointer, layout) };
    let cached = crate::require_some(allocator.stats());
    assert_eq!(cached.live_allocations, 0);
    assert_eq!(cached.cache.cached_objects, 1);
    assert_eq!(cached.cache.reclaimable_pages, Some(1));

    // SAFETY: The valid layout is paired with exact deallocation below.
    let reused = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert_eq!(reused, pointer);
    // SAFETY: `reused` is the exact live allocation returned above.
    unsafe { GlobalAlloc::dealloc(&*allocator, reused, layout) };
    assert!(allocator.reclaim_local_caches() > 0);
    assert_eq!(crate::require_some(allocator.stats()).slab_pages, 0);
    select_test_cpu(0);
}

#[test]
fn local_cache_preserves_cross_cpu_ownership_and_releases_empty_slab() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(2));
    let layout = crate::require_ok(Layout::from_size_align(64, 64));

    select_test_cpu(0);
    // SAFETY: The valid layout is paired with exact deallocations below.
    let first = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert!(!first.is_null());
    assert_eq!(
        crate::require_some(allocator.stats())
            .cache
            .reclaimable_pages,
        Some(0)
    );
    select_test_cpu(1);
    // SAFETY: Cross-CPU deallocation relinquishes the exact live allocation.
    unsafe { GlobalAlloc::dealloc(&*allocator, first, layout) };

    let cached = crate::require_some(allocator.stats());
    assert_eq!(cached.live_allocations, 0);
    assert_eq!(cached.cache.enabled_cpus, 2);
    assert_eq!(cached.cache.misses, 1);
    assert_eq!(cached.cache.refills, 1);
    assert_eq!(cached.allocation_requests, 1);
    assert!(cached.cache.cached_objects > 0);
    assert_eq!(cached.slab_pages, 1);
    assert_eq!(cached.cache.reclaimable_pages, Some(1));

    // SAFETY: The valid layout is paired with the exact deallocation below.
    let reused = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert_eq!(reused, first);
    // SAFETY: `reused` is the exact live allocation returned immediately above.
    unsafe { GlobalAlloc::dealloc(&*allocator, reused, layout) };
    assert_eq!(crate::require_some(allocator.stats()).cache.hits, 1);
    assert_eq!(
        crate::require_some(allocator.stats()).allocation_requests,
        2
    );

    assert!(allocator.reclaim_local_caches() > 0);
    let drained = crate::require_some(allocator.stats());
    assert_eq!(drained.cache.cached_objects, 0);
    assert_eq!(drained.cache.reclaimable_pages, Some(0));
    assert_eq!(drained.cache.pressure_reclaims, 0);
    assert!(drained.cache.reclaimed_objects > 0);
    assert_eq!(drained.slab_pages, 0);
    assert_eq!(drained.buddy.allocated_pages, 0);
    assert_eq!(drained.buddy.free_pages - cached.buddy.free_pages, 1);
    select_test_cpu(0);
}

#[test]
fn full_magazines_drain_without_duplicate_or_stranded_objects() {
    const CACHED_CLASSES: [usize; 6] = [16, 32, 64, 128, 256, 512];
    const MAX_CACHED_OBJECTS_PER_CPU: usize = 58;

    let (memory, handoff) = handoff(256);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(1));

    for size in CACHED_CLASSES {
        let layout = crate::require_ok(Layout::from_size_align(size, size));
        let mut pointers = Vec::new();
        let mut unique = HashSet::new();
        for _ in 0..40 {
            // SAFETY: Every successful allocation is retained and released
            // exactly once below with this layout.
            let pointer = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
            assert!(!pointer.is_null());
            assert!(unique.insert(pointer as usize));
            pointers.push(pointer);
        }
        // SAFETY: Every pointer is a distinct live allocation of this layout.
        unsafe {
            for pointer in pointers {
                GlobalAlloc::dealloc(&*allocator, pointer, layout);
            }
        }
        assert!(
            crate::require_some(allocator.stats()).cache.cached_objects
                <= MAX_CACHED_OBJECTS_PER_CPU
        );
    }
    let cached = crate::require_some(allocator.stats());
    assert!(cached.cache.drains > 0);
    assert_eq!(cached.live_allocations, 0);

    let central_layout = crate::require_ok(Layout::from_size_align(1024, 1024));
    let cached_before = cached.cache.cached_objects;
    // SAFETY: The allocation is paired with its exact deallocation below.
    let central = unsafe { GlobalAlloc::alloc(&*allocator, central_layout) };
    assert!(!central.is_null());
    // SAFETY: `central` is the exact live allocation returned above.
    unsafe { GlobalAlloc::dealloc(&*allocator, central, central_layout) };
    assert_eq!(
        crate::require_some(allocator.stats()).cache.cached_objects,
        cached_before
    );

    assert!(allocator.reclaim_local_caches() > 0);
    let reclaimed = crate::require_some(allocator.stats());
    assert_eq!(reclaimed.cache.cached_objects, 0);
    assert_eq!(reclaimed.slab_pages, 0);
    assert_eq!(reclaimed.buddy.allocated_pages, 0);
}

#[test]
fn page_pressure_reclaims_cached_slab_storage_before_failing() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(2));
    let layout = crate::require_ok(Layout::from_size_align(32, 32));

    select_test_cpu(1);
    // SAFETY: The allocation is paired with the exact deallocation below.
    let pointer = unsafe { GlobalAlloc::alloc(&*allocator, layout) };
    assert!(!pointer.is_null());
    // SAFETY: `pointer` is the exact live allocation returned immediately above.
    unsafe { GlobalAlloc::dealloc(&*allocator, pointer, layout) };
    assert!(crate::require_some(allocator.stats()).cache.cached_objects > 0);

    select_test_cpu(0);
    let all_memory = crate::require_ok(allocator.allocate_pages(6));
    let pressured = crate::require_some(allocator.stats());
    assert_eq!(pressured.cache.cached_objects, 0);
    assert_eq!(pressured.cache.pressure_reclaims, 1);
    assert_eq!(pressured.slab_pages, 0);

    // SAFETY: This is the exact live order-six allocation returned above.
    unsafe { crate::require_ok(allocator.deallocate_pages(all_memory, 6)) };
    assert_eq!(
        crate::require_some(allocator.stats()).buddy.allocated_pages,
        0
    );
}

#[test]
fn large_allocation_pressure_reclaims_remote_cached_slab() {
    let (memory, handoff) = handoff(32);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(2));
    let small = crate::require_ok(Layout::from_size_align(32, 32));

    select_test_cpu(1);
    // SAFETY: The allocation is paired with the exact deallocation below.
    let cached = unsafe { GlobalAlloc::alloc(&*allocator, small) };
    assert!(!cached.is_null());
    // SAFETY: `cached` is the exact live allocation returned above.
    unsafe { GlobalAlloc::dealloc(&*allocator, cached, small) };
    select_test_cpu(0);

    let large_layout = crate::require_ok(Layout::from_size_align(
        PAGE_SIZE as usize * 16 + 1,
        PAGE_SIZE as usize,
    ));
    // SAFETY: The valid layout is paired with exact deallocation below.
    let large = unsafe { GlobalAlloc::alloc(&*allocator, large_layout) };
    assert!(!large.is_null());
    let pressured = crate::require_some(allocator.stats());
    assert_eq!(pressured.cache.cached_objects, 0);
    assert_eq!(pressured.cache.pressure_reclaims, 1);
    // SAFETY: `large` is the exact live allocation returned above.
    unsafe { GlobalAlloc::dealloc(&*allocator, large, large_layout) };
    assert_eq!(
        crate::require_some(allocator.stats()).buddy.allocated_pages,
        0
    );
}

#[test]
fn unsupported_large_layout_does_not_reclaim_local_caches() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned test buffer is a stable writable direct map and
    // outlives the allocator and all allocations below.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    crate::require_ok(allocator.activate_local_caches(1));
    let small = crate::require_ok(Layout::from_size_align(16, 16));

    // SAFETY: The allocation is paired with the exact deallocation below.
    let pointer = unsafe { GlobalAlloc::alloc(&*allocator, small) };
    assert!(!pointer.is_null());
    // SAFETY: `pointer` is the exact live allocation returned immediately above.
    unsafe { GlobalAlloc::dealloc(&*allocator, pointer, small) };
    let before = crate::require_some(allocator.stats()).cache.cached_objects;

    let unsupported = crate::require_ok(Layout::from_size_align(
        (PAGE_SIZE as usize) << 19,
        PAGE_SIZE as usize,
    ));
    // SAFETY: The layout is valid for `GlobalAlloc`; null reports unsupported
    // capacity without creating an allocation that requires deallocation.
    assert!(unsafe { GlobalAlloc::alloc(&*allocator, unsupported) }.is_null());
    let after = crate::require_some(allocator.stats());
    assert_eq!(after.cache.cached_objects, before);
    assert_eq!(after.cache.pressure_reclaims, 0);
    assert!(allocator.reclaim_local_caches() > 0);
}

fn free_pages_from_blocks(blocks: &[usize]) -> usize {
    blocks
        .iter()
        .enumerate()
        .map(|(order, blocks)| blocks << order)
        .sum()
}

fn unused_invariant_handler(_report: AllocatorInvariantReport) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[test]
fn allocator_invariant_values_have_stable_diagnostics() {
    for code in 1..=16 {
        let invariant = crate::require_some(AllocatorInvariant::from_code(code));
        assert_eq!(invariant.code(), code);
        assert_ne!(invariant.description(), "unknown allocator invariant");
        assert!(format!("{invariant:?}").contains(invariant.description()));
    }
    assert!(AllocatorInvariant::from_code(0).is_none());
    assert!(AllocatorInvariant::from_code(17).is_none());
}

#[test]
fn allocator_invariant_handler_installation_is_process_wide_and_one_shot() {
    assert_eq!(
        install_allocator_invariant_handler(unused_invariant_handler),
        Ok(())
    );
    assert_eq!(
        install_allocator_invariant_handler(unused_invariant_handler),
        Err(AllocatorInvariantInstallError::AlreadyInstalled)
    );
}

#[test]
fn reclaimable_pages_cover_mixed_classes_and_cpus_without_draining() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned buffer outlives the allocator and every allocation.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let small = crate::require_ok(Layout::from_size_align(16, 16));
    let large = crate::require_ok(Layout::from_size_align(128, 128));
    // Allocate before activation to avoid prefilled magazines in this proof.
    // SAFETY: Both layouts are valid and paired with exact deallocations below.
    let first = unsafe { GlobalAlloc::alloc(&*allocator, small) };
    // SAFETY: The valid large layout is paired with its exact deallocation.
    let second = unsafe { GlobalAlloc::alloc(&*allocator, large) };
    assert!(!first.is_null() && !second.is_null());
    crate::require_ok(allocator.activate_local_caches(2));
    select_test_cpu(0);
    // SAFETY: The live first allocation is relinquished with its exact layout.
    unsafe { GlobalAlloc::dealloc(&*allocator, first, small) };
    select_test_cpu(1);
    // SAFETY: The live second allocation is relinquished with its exact layout.
    unsafe { GlobalAlloc::dealloc(&*allocator, second, large) };
    let before = crate::require_some(allocator.stats());
    let again = crate::require_some(allocator.stats());
    assert_eq!(before.cache.cached_objects, 2);
    assert_eq!(before.cache.reclaimable_pages, Some(2));
    assert_eq!(before, again);
    assert_eq!(allocator.reclaim_local_caches(), 2);
    let drained = crate::require_some(allocator.stats());
    assert_eq!(drained.cache.reclaimable_pages, Some(0));
    assert_eq!(drained.buddy.allocated_pages, 0);
    select_test_cpu(0);
}

#[test]
fn cache_reservations_account_for_pending_and_constructed_metadata() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The direct map remains writable until all allocations are released.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let pending = crate::require_some(allocator.try_reserve_cache_metadata(8, 8));
    let layout = crate::require_ok(Layout::from_size_align(4096, 8));
    assert_eq!(allocation_page_bound(layout), Some(2));
    // SAFETY: The returned allocation is freed below with its exact layout.
    let metadata = unsafe { allocator.alloc(layout) };
    assert!(!metadata.is_null());
    let available = crate::require_some(allocator.page_availability());
    assert_eq!(available.free_pages, 62);
    assert_eq!(available.pending_cache_metadata_pages, 8);
    assert_eq!(available.available_for_cache(), 54);

    let mut pages = Vec::new();
    while let Ok(page) = allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, 8) {
        pages.push(page);
    }
    assert_eq!(pages.len(), 46);
    assert!(allocator.try_reserve_cache_metadata(1, 8).is_none());
    // A completed/rolled-back construction releases its conservative claim.
    // SAFETY: Exact live allocation/layout, no remaining references.
    unsafe { allocator.dealloc(metadata, layout) };
    drop(pending);
    assert_eq!(
        crate::require_some(allocator.page_availability()).available_for_cache(),
        18
    );
    let ordinary = crate::require_ok(allocator.allocate_pages_for(0, PageOwner::User));
    // Ordinary allocations have priority over cache headroom.
    assert_eq!(
        crate::require_some(allocator.page_availability()).free_pages,
        17
    );
    // SAFETY: Every address is released exactly once with its original owner.
    unsafe {
        for page in pages {
            crate::require_ok(allocator.deallocate_pages_for(page, 0, PageOwner::FileCache));
        }
        crate::require_ok(allocator.deallocate_pages_for(ordinary, 0, PageOwner::User));
    }
    assert_eq!(
        crate::require_some(allocator.page_availability()).free_pages,
        64
    );
    assert!(
        allocator
            .try_reserve_cache_metadata(usize::MAX, 1)
            .is_none()
    );
    assert_eq!(
        allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, usize::MAX),
        Err(BuddyError::OutOfMemory)
    );
    assert_eq!(
        allocator.allocate_pages_above_reserve(usize::MAX, PageOwner::FileCache, 0),
        Err(BuddyError::InvalidOrder)
    );
}

#[test]
fn concurrent_cache_metadata_claims_cannot_overbook_headroom() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: Scoped workers finish before the direct map is released.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let rendezvous = std::sync::Barrier::new(9);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let allocator = &allocator;
            let rendezvous = &rendezvous;
            scope.spawn(move || {
                let claim = allocator.try_reserve_cache_metadata(8, 8);
                rendezvous.wait();
                rendezvous.wait();
                drop(claim);
            });
        }
        rendezvous.wait();
        let available = allocator.page_availability();
        let denied = allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, 8);
        rendezvous.wait();
        let available = crate::require_some(available);
        assert_eq!(available.pending_cache_metadata_pages, 56);
        assert_eq!(available.free_pages, 64);
        assert_eq!(available.available_for_cache(), 8);
        assert_eq!(denied, Err(BuddyError::OutOfMemory));
    });
    assert_eq!(
        crate::require_some(allocator.page_availability()).pending_cache_metadata_pages,
        0
    );
}

#[test]
fn concurrent_cache_pages_preserve_reserve_and_ordinary_allocations_can_use_it() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: Scoped workers finish before the direct map is released.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let pages = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..8 {
            let allocator = &allocator;
            workers.push(scope.spawn(move || {
                let mut pages = Vec::new();
                while let Ok(page) =
                    allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, 8)
                {
                    pages.push(page);
                }
                pages
            }));
        }
        workers
            .into_iter()
            .flat_map(|worker| crate::require_ok(worker.join()))
            .collect::<Vec<_>>()
    });
    assert_eq!(pages.len(), 56);
    let available = crate::require_some(allocator.page_availability());
    assert_eq!(available.free_pages, 8);
    assert_eq!(available.file_cache_pages, 56);
    let mut ordinary = Vec::new();
    for _ in 0..8 {
        ordinary.push(crate::require_ok(
            allocator.allocate_pages_for(0, PageOwner::User),
        ));
    }
    assert_eq!(
        crate::require_some(allocator.page_availability()).free_pages,
        0
    );
    // SAFETY: Exact live blocks, each released once after all workers finished.
    unsafe {
        for page in pages {
            crate::require_ok(allocator.deallocate_pages_for(page, 0, PageOwner::FileCache));
        }
        for page in ordinary {
            crate::require_ok(allocator.deallocate_pages_for(page, 0, PageOwner::User));
        }
    }
}

#[test]
fn allocation_backing_bound_covers_headers_alignment_and_all_classes() {
    assert_eq!(
        allocation_page_bound(crate::require_ok(Layout::from_size_align(0, 1))),
        Some(1)
    );
    for size in [16, 32, 64, 128, 256, 512, 1024, 2048, 2049, 4096, 9000] {
        for align in [1, 8, 64, 4096] {
            let (memory, handoff) = handoff(64);
            let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
            // SAFETY: This iteration owns the direct map and all allocations.
            crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
            crate::require_ok(allocator.activate_local_caches(1));
            let layout = crate::require_ok(Layout::from_size_align(size, align));
            let bound = crate::require_some(allocation_page_bound(layout));
            // SAFETY: Each allocation is freed with its exact layout below.
            unsafe {
                let pointer = allocator.alloc(layout);
                assert!(!pointer.is_null());
                assert!(
                    64 - crate::require_some(allocator.page_availability()).free_pages <= bound
                );
                allocator.dealloc(pointer, layout);
            }
            allocator.reclaim_local_caches();
        }
    }
    let unsupported = crate::require_ok(Layout::from_size_align(
        (1usize << hyper::mm::MAX_ORDER) * PAGE_SIZE as usize,
        1,
    ));
    assert_eq!(allocation_page_bound(unsupported), None);
}

struct ReclaimPolicy;
type ReclaimAllocator = KernelGlobalAllocator<ReclaimPolicy>;

std::thread_local! {
    static RECLAIM_CALLS: Cell<usize> = const { Cell::new(0) };
    static RECLAIM_REQUEST: Cell<usize> = const { Cell::new(0) };
    static PRESSURE_OBSERVATION: Cell<Option<PageAvailability>> = const { Cell::new(None) };
    static RELEASE_OBSERVATION: Cell<Option<PageAvailability>> = const { Cell::new(None) };
    static RECLAIM_BLOCK: Cell<Option<(core::ptr::NonNull<ReclaimAllocator>, hyper::mm::PhysicalAddress, usize)>> = const { Cell::new(None) };
}

impl InterruptMask for ReclaimPolicy {
    type State = bool;

    fn save_and_disable() -> bool {
        TestInterruptMask::save_and_disable()
    }

    fn restore(state: bool) {
        TestInterruptMask::restore(state);
    }
}

// SAFETY: Delegates to the same synchronous host CPU pin and interrupt mask.
unsafe impl CpuLocalCachePolicy for ReclaimPolicy {
    type Pin = TestPin;

    fn pin() -> Option<Self::Pin> {
        TestInterruptMask::pin()
    }

    fn current_cpu(pin: &Self::Pin) -> Option<hyper::cpu::CpuIndex> {
        TestInterruptMask::current_cpu(pin)
    }

    fn memory_pressure(available: PageAvailability) {
        assert!(test_irq_masked());
        PRESSURE_OBSERVATION.with(|last| last.set(Some(available)));
    }

    fn memory_released(available: PageAvailability) {
        assert!(test_irq_masked());
        RELEASE_OBSERVATION.with(|last| last.set(Some(available)));
    }

    fn try_reclaim(pages: usize) -> usize {
        assert!(!test_irq_masked());
        TEST_PIN_DEPTH.with(|depth| assert_eq!(depth.get(), 0));
        RECLAIM_CALLS.with(|count| count.set(count.get() + 1));
        RECLAIM_REQUEST.with(|request| request.set(pages));
        let Some((allocator, address, order)) = RECLAIM_BLOCK.with(|block| block.take()) else {
            return 0;
        };
        // SAFETY: Each test installs this pointer only while its allocator and
        // exact live cache block exist; this take is their sole release path.
        let allocator = unsafe { allocator.as_ref() };
        assert!(allocator.page_availability().is_some());
        // Re-entry proves the callback runs without the central lock held.
        // SAFETY: The installed block is unique and has the recorded owner/order.
        crate::require_ok(unsafe {
            allocator.deallocate_pages_for(address, order, PageOwner::FileCache)
        });
        1usize << order
    }
}

#[test]
fn ordinary_oom_reclaims_once_outside_allocator_locks_for_all_paths() {
    for cached in [false, true] {
        for object_size in [0, 64, 4096] {
            set_test_irq_masked(false);
            RECLAIM_CALLS.with(|count| count.set(0));
            let (memory, handoff) = handoff(64);
            let allocator = ManuallyDrop::new(ReclaimAllocator::new());
            // SAFETY: The allocator and direct map outlive callback registration.
            crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
            if cached {
                crate::require_ok(allocator.activate_local_caches(1));
            }
            let reclaim_order = usize::from(object_size == 4096);
            let block = crate::require_ok(
                allocator.allocate_pages_for(reclaim_order, PageOwner::FileCache),
            );
            let mut occupied = Vec::new();
            for _ in 0..64 - (1 << reclaim_order) {
                occupied.push(crate::require_ok(
                    allocator.allocate_pages_for(0, PageOwner::User),
                ));
            }
            assert_eq!(
                crate::require_some(PRESSURE_OBSERVATION.with(Cell::get)).free_pages,
                0
            );
            RECLAIM_BLOCK.with(|pending| {
                pending.set(Some((
                    core::ptr::NonNull::from(&*allocator),
                    block,
                    reclaim_order,
                )))
            });
            // Protected cache allocation must never recurse into reclamation.
            assert_eq!(
                allocator.allocate_pages_above_reserve(0, PageOwner::FileCache, 0),
                Err(BuddyError::OutOfMemory)
            );
            assert_eq!(RECLAIM_CALLS.with(Cell::get), 0);
            if object_size == 0 {
                let page = crate::require_ok(allocator.allocate_pages_for(0, PageOwner::User));
                // SAFETY: Exact live block just returned, no remaining users.
                crate::require_ok(unsafe {
                    allocator.deallocate_pages_for(page, 0, PageOwner::User)
                });
            } else {
                let layout = crate::require_ok(Layout::from_size_align(object_size, 8));
                // SAFETY: The allocation is freed exactly once with its layout.
                unsafe {
                    let pointer = allocator.alloc(layout);
                    assert!(!pointer.is_null());
                    allocator.dealloc(pointer, layout);
                }
                allocator.reclaim_local_caches();
            }
            assert_eq!(RECLAIM_CALLS.with(Cell::get), 1);
            assert_eq!(
                RECLAIM_REQUEST.with(Cell::get),
                if object_size == 4096 { 2 } else { 1 }
            );
            assert!(RECLAIM_BLOCK.with(Cell::get).is_none());
            // SAFETY: These independent live blocks were never offered for reclaim.
            unsafe {
                for page in occupied {
                    crate::require_ok(allocator.deallocate_pages_for(page, 0, PageOwner::User));
                }
            }
            assert_eq!(
                crate::require_some(allocator.page_availability()).free_pages,
                64
            );
        }
    }
}

#[test]
fn failed_emergency_reclaim_is_bounded_even_for_large_requests() {
    set_test_irq_masked(false);
    RECLAIM_CALLS.with(|count| count.set(0));
    RECLAIM_BLOCK.with(|pending| pending.set(None));
    let (memory, handoff) = handoff(256);
    let allocator = ManuallyDrop::new(ReclaimAllocator::new());
    // SAFETY: The direct map outlives every block allocated in this test.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let occupied = crate::require_ok(allocator.allocate_pages_for(8, PageOwner::User));
    assert_eq!(
        allocator.allocate_pages_for(7, PageOwner::Guest),
        Err(BuddyError::OutOfMemory)
    );
    assert_eq!(RECLAIM_CALLS.with(Cell::get), 1);
    assert_eq!(RECLAIM_REQUEST.with(Cell::get), 64);
    assert_eq!(
        allocator.allocate_pages_for(usize::MAX, PageOwner::User),
        Err(BuddyError::InvalidOrder)
    );
    assert_eq!(RECLAIM_CALLS.with(Cell::get), 1);
    // SAFETY: Exact live block, released once.
    crate::require_ok(unsafe { allocator.deallocate_pages_for(occupied, 8, PageOwner::User) });
}

#[test]
fn finite_cache_sweep_restores_a_large_buddy_order_above_resume_watermark() {
    use crate::cache_reclaim_requests::RequestState;
    use crate::file_data_cache::{
        CacheAccess, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePageIndex,
        FilesystemGeneration, NodeIdentity, ReclaimCursor, Refill,
    };
    use core::num::NonZeroU64;

    struct Page<'a> {
        allocator: &'a KernelGlobalAllocator<TestInterruptMask>,
        address: hyper::mm::PhysicalAddress,
    }
    impl Drop for Page<'_> {
        fn drop(&mut self) {
            // SAFETY: This owner is the sole release path for its order-0 block.
            crate::require_ok(unsafe {
                self.allocator
                    .deallocate_pages_for(self.address, 0, PageOwner::FileCache)
            });
        }
    }

    set_test_irq_masked(false);
    let (memory, handoff) = handoff(1024);
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The test direct map outlives every cache and ordinary page owner.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let mut blocks = Vec::new();
    for _ in 0..1024 {
        blocks.push(crate::require_ok(
            allocator.allocate_pages_for(0, PageOwner::FileCache),
        ));
    }
    let cache = crate::require_ok(FileDataCache::try_new(768));
    let file = FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MIN),
        NodeIdentity::new(NonZeroU64::MIN),
        ContentRevision::new(NonZeroU64::MIN),
    );
    for (index, address) in blocks.into_iter().enumerate() {
        let page = Page {
            allocator: &allocator,
            address,
        };
        if index.is_multiple_of(4) {
            drop(page);
            continue;
        }
        let key = CacheKey::new(file, FilePageIndex::new(index as u64));
        let load = match crate::require_ok(cache.access(key)) {
            CacheAccess::Load(load) => load,
            _ => panic!("fixture cache admission failed"),
        };
        drop(crate::require_ok(
            load.fill_and_publish(|| Ok(page), |_| Refill::Recreate),
        ));
    }
    let before = crate::require_some(allocator.page_availability());
    assert_eq!(before.free_pages, 256);
    assert!(before.free_pages * 100 > before.managed_pages * 15);
    assert_eq!(before.largest_free_order, Some(0));
    assert_eq!(
        allocator.allocate_pages_for(8, PageOwner::User),
        Err(BuddyError::OutOfMemory)
    );

    let pause = cache.pause_admission();
    let mut requests = RequestState::new();
    let request = crate::require_some(requests.begin(8));
    let mut cursor = ReclaimCursor::new();
    let first = cache.reclaim_scan(&mut cursor, 64, |_| true);
    assert_eq!(first.detached, 64);
    assert!(!first.finished);
    assert!(
        crate::require_some(allocator.page_availability())
            .largest_free_order
            .is_none_or(|order| order < request.target)
    );
    let mut detached = first.detached;
    loop {
        let batch = cache.reclaim_scan(&mut cursor, 64, |_| true);
        assert!(batch.inspected <= 64);
        detached += batch.detached;
        let current = crate::require_some(allocator.page_availability());
        assert!(current.free_pages * 100 > current.managed_pages * 15);
        if current
            .largest_free_order
            .is_some_and(|order| order >= request.target)
        {
            break;
        }
        assert!(
            !batch.finished,
            "reclaimable fragmented block was not restored"
        );
    }
    assert!(detached > 64);
    assert!(requests.finish(request.generation));
    let allocated = crate::require_ok(allocator.allocate_pages_for(8, PageOwner::User));
    assert!(cache.usage().admission_paused);
    drop(pause);
    // SAFETY: The successful requested block is no longer used by this test.
    crate::require_ok(unsafe { allocator.deallocate_pages_for(allocated, 8, PageOwner::User) });
    drop(cache);
    assert_eq!(
        crate::require_some(allocator.page_availability()).free_pages,
        1024
    );
}

#[test]
fn cache_availability_excludes_boot_reserved_ram() {
    let memory_buffer = AlignedMemory::new(64);
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(0, 64 * PAGE_SIZE))));
    let reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    let mut boot = crate::require_ok(BootAllocator::new(&memory, &reserved, 64 * PAGE_SIZE));
    crate::require_ok(boot.reserve(crate::require_some(PhysicalRange::new(0, 16 * PAGE_SIZE))));
    let allocator = ManuallyDrop::new(KernelGlobalAllocator::<TestInterruptMask>::new());
    // SAFETY: The aligned buffer maps all RAM; the handoff excludes reserved pages.
    crate::require_ok(unsafe {
        allocator.initialize(&boot.handoff(), memory_buffer.pointer as u64)
    });
    let available = crate::require_some(allocator.page_availability());
    assert_eq!(available.managed_pages, 48);
    assert_eq!(available.free_pages, 48);
    assert!(allocator.try_reserve_cache_metadata(41, 8).is_none());
    let claim = crate::require_some(allocator.try_reserve_cache_metadata(40, 8));
    assert_eq!(
        crate::require_some(allocator.page_availability()).available_for_cache(),
        8
    );
    drop(claim);
}

#[test]
fn memory_release_notifies_after_page_heap_and_metadata_headroom_returns() {
    let (memory, handoff) = handoff(64);
    let allocator = ManuallyDrop::new(ReclaimAllocator::new());
    // SAFETY: All allocations and callbacks finish before this mapping expires.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let page = crate::require_ok(allocator.allocate_pages_for(2, PageOwner::User));
    RELEASE_OBSERVATION.with(|last| last.set(None));
    // SAFETY: Exact live user block, no remaining references.
    crate::require_ok(unsafe { allocator.deallocate_pages_for(page, 2, PageOwner::User) });
    assert_eq!(
        crate::require_some(RELEASE_OBSERVATION.with(Cell::get)).free_pages,
        64
    );
    for cached in [false, true] {
        if cached {
            crate::require_ok(allocator.activate_local_caches(1));
        }
        for size in [64, 4096] {
            let layout = crate::require_ok(Layout::from_size_align(size, 8));
            // SAFETY: The exact layout and allocation are paired within this block.
            unsafe {
                let pointer = allocator.alloc(layout);
                assert!(!pointer.is_null());
                RELEASE_OBSERVATION.with(|last| last.set(None));
                allocator.dealloc(pointer, layout);
            }
            if cached && size == 64 {
                assert!(RELEASE_OBSERVATION.with(Cell::get).is_none());
                allocator.reclaim_local_caches();
            }
            assert_eq!(
                crate::require_some(RELEASE_OBSERVATION.with(Cell::get)).free_pages,
                64
            );
        }
    }
    let claim = crate::require_some(allocator.try_reserve_cache_metadata(12, 8));
    RELEASE_OBSERVATION.with(|last| last.set(None));
    drop(claim);
    let release = crate::require_some(RELEASE_OBSERVATION.with(Cell::get));
    assert_eq!(release.available_for_cache(), 64);
    assert_eq!(release.pending_cache_metadata_pages, 0);
}

#[test]
fn partial_emergency_reclaim_does_not_loop_until_a_large_request_succeeds() {
    set_test_irq_masked(false);
    RECLAIM_CALLS.with(|count| count.set(0));
    let (memory, handoff) = handoff(256);
    let allocator = ManuallyDrop::new(ReclaimAllocator::new());
    // SAFETY: The direct map outlives callback registration and all live blocks.
    crate::require_ok(unsafe { allocator.initialize(&handoff, memory.pointer as u64) });
    let reclaimable = crate::require_ok(allocator.allocate_pages_for(0, PageOwner::FileCache));
    let mut occupied = Vec::new();
    for _ in 0..255 {
        occupied.push(crate::require_ok(
            allocator.allocate_pages_for(0, PageOwner::User),
        ));
    }
    RECLAIM_BLOCK.with(|pending| {
        pending.set(Some((
            core::ptr::NonNull::from(&*allocator),
            reclaimable,
            0,
        )))
    });
    assert_eq!(
        allocator.allocate_pages_for(7, PageOwner::Guest),
        Err(BuddyError::OutOfMemory)
    );
    assert_eq!(RECLAIM_CALLS.with(Cell::get), 1);
    assert_eq!(RECLAIM_REQUEST.with(Cell::get), 64);
    assert!(RECLAIM_BLOCK.with(Cell::get).is_none());
    assert_eq!(
        crate::require_some(allocator.page_availability()).free_pages,
        1
    );
    // SAFETY: All user blocks remain live and independent of the reclaimed page.
    unsafe {
        for page in occupied {
            crate::require_ok(allocator.deallocate_pages_for(page, 0, PageOwner::User));
        }
    }
}
