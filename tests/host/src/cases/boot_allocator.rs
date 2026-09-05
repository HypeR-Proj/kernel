// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Early page-allocation range, reservation, and accounting contracts.

use hyper::mm::{BootAllocator, BootAllocatorError, PAGE_SIZE};
use hyper::platform::{MAX_MEMORY_REGIONS, MAX_RESERVED_REGIONS, PhysicalRange, RegionList};

#[test]
fn skips_reservations_and_records_allocations() {
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE,
        PAGE_SIZE * 8,
    ))));
    let mut reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    crate::require_ok(reserved.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE * 2,
        PAGE_SIZE * 2,
    ))));
    let mut allocator = crate::require_ok(BootAllocator::new(&memory, &reserved, PAGE_SIZE * 16));

    assert_eq!(
        crate::require_ok(allocator.allocate_pages(1, 1)).get(),
        PAGE_SIZE
    );
    assert_eq!(
        crate::require_ok(allocator.allocate_pages(1, 1)).get(),
        PAGE_SIZE * 4
    );
    assert_eq!(allocator.reservations().len(), 1);
    assert_eq!(allocator.reservations()[0].start(), PAGE_SIZE);
    assert_eq!(allocator.reservations()[0].size(), PAGE_SIZE * 4);
}

#[test]
fn honors_multi_page_alignment() {
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE,
        PAGE_SIZE * 16,
    ))));
    let reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    let mut allocator = crate::require_ok(BootAllocator::new(&memory, &reserved, PAGE_SIZE * 32));

    assert_eq!(
        crate::require_ok(allocator.allocate_pages(2, 4)).get(),
        PAGE_SIZE * 4
    );
}

#[test]
fn rejects_invalid_requests_and_honors_the_accessible_limit() {
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE,
        PAGE_SIZE * 8,
    ))));
    let reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    let mut allocator = crate::require_ok(BootAllocator::new(&memory, &reserved, PAGE_SIZE * 3));

    assert_eq!(
        allocator.allocate_pages(0, 1),
        Err(BootAllocatorError::InvalidRequest)
    );
    assert_eq!(
        allocator.allocate_pages(1, 3),
        Err(BootAllocatorError::InvalidAlignment)
    );
    assert_eq!(
        crate::require_ok(allocator.allocate_pages(2, 1)).get(),
        PAGE_SIZE
    );
    assert_eq!(
        allocator.allocate_pages(1, 1),
        Err(BootAllocatorError::OutOfMemory)
    );
}

#[test]
fn reports_only_reserved_pages_that_overlap_ram() {
    let mut memory = RegionList::<MAX_MEMORY_REGIONS>::new();
    crate::require_ok(memory.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE,
        PAGE_SIZE * 8,
    ))));
    let mut reserved = RegionList::<MAX_RESERVED_REGIONS>::new();
    crate::require_ok(reserved.insert(crate::require_some(PhysicalRange::new(0, PAGE_SIZE * 3))));
    crate::require_ok(reserved.insert(crate::require_some(PhysicalRange::new(
        PAGE_SIZE * 32,
        PAGE_SIZE,
    ))));
    let allocator = crate::require_ok(BootAllocator::new(&memory, &reserved, PAGE_SIZE * 64));

    let stats = allocator.stats();
    assert_eq!(stats.ram_pages, 8);
    assert_eq!(stats.reserved_pages, 2);
    assert_eq!(stats.available_pages, 6);
}
