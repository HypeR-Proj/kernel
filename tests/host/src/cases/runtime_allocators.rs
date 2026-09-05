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
    CacheActivationError, CpuLocalCachePolicy, InitError, KernelGlobalAllocator, PageOwner,
    SlabAllocator, install_allocator_invariant_handler,
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
    let stats = crate::require_some(allocator.stats());
    assert_eq!(stats.guest_pages.pages, 8);
    assert_eq!(stats.page_table_pages.pages, 1);
    assert_eq!(stats.user_pages.pages, 2);
    assert_eq!(stats.buddy.allocated_pages, 11);

    // SAFETY: These are the exact live blocks and owners returned above.
    unsafe {
        crate::require_ok(allocator.deallocate_pages_for(table, 0, PageOwner::PageTable));
        crate::require_ok(allocator.deallocate_pages_for(user, 1, PageOwner::User));
        crate::require_ok(allocator.deallocate_pages_for(guest, 3, PageOwner::Guest));
    }
    let stats = crate::require_some(allocator.stats());
    assert_eq!(stats.guest_pages.pages, 0);
    assert_eq!(stats.guest_pages.peak_pages, 8);
    assert_eq!(stats.page_table_pages.pages, 0);
    assert_eq!(stats.user_pages.pages, 0);
    assert_eq!(stats.user_pages.peak_pages, 2);
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
    assert_eq!(drained.cache.pressure_reclaims, 0);
    assert!(drained.cache.reclaimed_objects > 0);
    assert_eq!(drained.slab_pages, 0);
    assert_eq!(drained.buddy.allocated_pages, 0);
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
