// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pressure admission for metadata; physical payloads use the page allocator.

use core::alloc::Layout;

use super::CacheError;

#[cfg(not(test))]
pub(super) type Reservation = crate::mm::cache_memory::MetadataReservation;
#[cfg(test)]
pub(super) struct Reservation;

#[cfg(test)]
impl Drop for Reservation {
    fn drop(&mut self) {
        // Host allocations have no physical admission counter, but keep the
        // same explicit reservation lifetime as the production RAII token.
    }
}

pub(super) fn reserve(layouts: &[Layout]) -> Result<(Reservation, usize), CacheError> {
    reserve_table(layouts, false)
}

pub(super) fn reserve_table(
    layouts: &[Layout],
    shrinking: bool,
) -> Result<(Reservation, usize), CacheError> {
    let mut pages = 0_usize;
    for &layout in layouts {
        let bound = allocation_pages(layout).ok_or(CacheError::InvalidCapacity)?;
        pages = pages
            .checked_add(bound)
            .ok_or(CacheError::InvalidCapacity)?;
    }
    #[cfg(not(test))]
    let reservation = if shrinking {
        // A smaller replacement must be able to release a larger old index
        // below the admission watermark. It remains fallible and never
        // invokes reclaim recursively from the reclaim worker.
        crate::mm::allocator::policy::GLOBAL_ALLOCATOR.try_reserve_cache_metadata(pages, 0)
    } else {
        crate::mm::cache_memory::reserve_metadata(pages)
    }
    .ok_or(CacheError::Allocation)?;
    #[cfg(test)]
    let reservation = {
        let _ = shrinking;
        Reservation
    };
    Ok((reservation, pages))
}

fn allocation_pages(layout: Layout) -> Option<usize> {
    #[cfg(not(test))]
    {
        crate::mm::cache_memory::allocation_page_bound(layout)
    }
    #[cfg(test)]
    {
        let page = hyper::mm::PAGE_SIZE as usize;
        layout
            .size()
            .max(layout.align())
            .max(1)
            .checked_add(page - 1)
            .and_then(|bytes| (bytes / page).checked_next_power_of_two())
    }
}

pub(super) fn admission_allowed() -> bool {
    #[cfg(not(test))]
    {
        crate::mm::cache_memory::admission_allowed()
    }
    #[cfg(test)]
    true
}

/// Fixed-capacity cache arrays use a distinct allocator pool. They remain Vecs
/// so normal global destruction handles rollback; callers never grow them.
pub(super) fn vector<T>(capacity: usize) -> Result<alloc::vec::Vec<T>, super::state::Error> {
    let layout = Layout::array::<T>(capacity).map_err(|_| super::state::Error::InvalidCapacity)?;
    if layout.size() == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let pointer = allocate(layout).cast::<T>();
    if pointer.is_null() {
        return Err(super::state::Error::Allocation);
    }
    // SAFETY: This exclusive global-compatible allocation has exactly the
    // requested array layout. No elements are initialized until Vec writes them.
    Ok(unsafe { alloc::vec::Vec::from_raw_parts(pointer, 0, capacity) })
}

pub(super) fn shared<T>(value: T) -> Result<hyper::mm::FallibleArc<T>, CacheError> {
    // SAFETY: allocate returns storage from the installed global allocator's
    // tagged cache pool; ordinary global deallocation recognizes that tag.
    unsafe { hyper::mm::FallibleArc::try_new_or_return_in(value, allocate) }
        .map_err(|_| CacheError::Allocation)
}

fn allocate(layout: Layout) -> *mut u8 {
    #[cfg(not(test))]
    {
        crate::mm::allocator::policy::GLOBAL_ALLOCATOR.allocate_file_cache(layout)
    }
    #[cfg(test)]
    // SAFETY: The checked layout is passed directly to the host global allocator.
    unsafe {
        alloc::alloc::alloc(layout)
    }
}
