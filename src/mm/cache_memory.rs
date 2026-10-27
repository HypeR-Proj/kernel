// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Opportunistic file-cache backing over managed, immediately free RAM.
//!
//! The worker owns pressure hysteresis; every backing allocation additionally
//! checks the stop reserve under the central allocator lock. Boot reservations
//! and CPU-magazine objects are never counted as immediately available RAM.

use crate::mm::allocator::BuddyError;
use {
    crate::mm::allocator::heap::CacheMetadataReservation,
    crate::mm::allocator::heap::PageAvailability,
};

use super::page_block::PageBlock;
use {
    crate::mm::allocator::policy::GLOBAL_ALLOCATOR,
    crate::mm::allocator::policy::KernelAllocatorPolicy,
};

pub(crate) use crate::mm::allocator::heap::allocation_page_bound;

pub(crate) type MetadataReservation = CacheMetadataReservation<'static, KernelAllocatorPolicy>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Watermarks {
    pub stop_pages: usize,
    pub resume_pages: usize,
}

pub(crate) fn watermarks(managed_pages: usize) -> Watermarks {
    let small_memory_floor =
        (4 * 1024 * 1024 / hyper::mm::PAGE_SIZE as usize).min(managed_pages / 4);
    let stop_pages = managed_pages.div_ceil(10).max(small_memory_floor);
    let fifteen_percent = (managed_pages / 20) * 3 + ((managed_pages % 20) * 3).div_ceil(20);
    // Retain a real gap when the absolute floor dominates on small machines.
    let gap = (managed_pages / 20).clamp(1, 64);
    let resume_pages = fifteen_percent
        .max(stop_pages.saturating_add(gap))
        .min(managed_pages);
    Watermarks {
        stop_pages,
        resume_pages,
    }
}

pub(crate) fn availability() -> Option<PageAvailability> {
    GLOBAL_ALLOCATOR.page_availability()
}

/// Advisory early rejection; allocating callers still perform an atomic check.
pub(crate) fn admission_allowed() -> bool {
    availability().is_some_and(|memory| {
        memory.available_for_cache() > watermarks(memory.managed_pages).stop_pages
    })
}

pub(crate) fn allocate_page() -> Result<PageBlock, BuddyError> {
    let memory = availability().ok_or(BuddyError::OutOfMemory)?;
    PageBlock::allocate_cache(watermarks(memory.managed_pages).stop_pages)
}

pub(crate) fn reserve_metadata(pages: usize) -> Option<MetadataReservation> {
    let memory = availability()?;
    GLOBAL_ALLOCATOR.try_reserve_cache_metadata(pages, watermarks(memory.managed_pages).stop_pages)
}

/// Called with the allocator lock held: only enqueue deferred work here.
pub(super) fn observe_pressure(memory: PageAvailability) {
    if memory.available_for_cache() <= watermarks(memory.managed_pages).stop_pages {
        crate::fs::cache::worker::request();
    }
}
