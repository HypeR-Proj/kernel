// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only allocation diagnostics; no heap flush, reclamation or observer
//! state is introduced by a request. Snapshots never hold allocator and cache
//! locks together and never return pointers or operational authority.

use {
    crate::mm::allocator::heap::HeapStats, crate::mm::allocator::heap::PageOwnerStats,
    crate::mm::allocator::heap::profile::HeapClassStats,
};

#[derive(Clone, Copy)]
pub(crate) enum Query {
    Heap,
    Buddy(usize),
    Owner(usize),
    FileCache,
    HeapClass(usize),
}

pub(crate) struct Statistics {
    pub(crate) captured_at_ns: u64,
    pub(crate) data: Data,
}

pub(crate) enum Data {
    Heap(HeapSummary),
    Buddy {
        order: usize,
        free_blocks: usize,
    },
    Owner(PageOwnerStats),
    FileCache {
        usage: crate::fs::cache::CacheUsage,
        payload_pages: usize,
        slab_pages: usize,
        large_pages: usize,
    },
    HeapClass(HeapClassStats),
}

impl super::MemoryInspector {
    pub(crate) fn statistics(&self, query: Query) -> Result<Statistics, super::Error> {
        let allocator = &crate::mm::allocator::policy::GLOBAL_ALLOCATOR;
        let data = match query {
            Query::HeapClass(index) => {
                if !cfg!(feature = "heap-profiling") {
                    return Err(super::Error::Unsupported);
                }
                Data::HeapClass(
                    allocator
                        .heap_class_stats(index)
                        .ok_or(super::Error::Unavailable)?,
                )
            }
            _ => {
                let stats = allocator
                    .accounting_stats()
                    .ok_or(super::Error::Unavailable)?;
                match query {
                    Query::Heap => Data::Heap(HeapSummary::from(stats)),
                    Query::Buddy(order) => Data::Buddy {
                        order,
                        free_blocks: *stats
                            .buddy
                            .free_blocks
                            .get(order)
                            .ok_or(super::Error::NotFound)?,
                    },
                    Query::Owner(index) => {
                        let owners = [
                            stats.kernel_pages,
                            stats.page_table_pages,
                            stats.guest_pages,
                            stats.user_pages,
                            stats.file_cache_pages,
                        ];
                        Data::Owner(*owners.get(index).ok_or(super::Error::NotFound)?)
                    }
                    Query::FileCache => Data::FileCache {
                        usage: crate::fs::vfs::file_cache()
                            .ok_or(super::Error::Unavailable)?
                            .usage(),
                        payload_pages: stats.file_cache_pages.pages,
                        slab_pages: stats.file_cache_slab_pages,
                        large_pages: stats.file_cache_large_pages,
                    },
                    Query::HeapClass(_) => return Err(super::Error::Unsupported),
                }
            }
        };
        Ok(Statistics {
            captured_at_ns: crate::time::monotonic_nanoseconds().unwrap_or_default(),
            data,
        })
    }
}

pub(crate) struct HeapSummary {
    pub(crate) live_objects: u64,
    pub(crate) live_slab_objects: u64,
    pub(crate) live_large_objects: u64,
    pub(crate) requested_bytes: u64,
    pub(crate) peak_objects: u64,
    pub(crate) peak_requested_bytes: u64,
    pub(crate) allocations: u64,
    pub(crate) failures: u64,
    pub(crate) cached_objects: u64,
    pub(crate) cache_hits: u64,
    pub(crate) cache_misses: u64,
    pub(crate) cache_refills: u64,
    pub(crate) cache_drains: u64,
    pub(crate) pressure_reclaims: u64,
    pub(crate) reclaimed_objects: u64,
    pub(crate) cache_cpus: u64,
    pub(crate) profiling_enabled: u64,
    pub(crate) page_allocations: u64,
    pub(crate) page_failures: u64,
    pub(crate) page_deallocations: u64,
    pub(crate) peak_allocated_pages: u64,
}
impl From<HeapStats> for HeapSummary {
    fn from(s: HeapStats) -> Self {
        Self {
            live_objects: s.live_allocations as u64,
            live_slab_objects: s.live_slab_allocations as u64,
            live_large_objects: s.live_large_allocations as u64,
            requested_bytes: s.requested_bytes as u64,
            peak_objects: s.peak_live_allocations as u64,
            peak_requested_bytes: s.peak_requested_bytes as u64,
            allocations: s.allocation_requests,
            failures: s.allocation_failures,
            cached_objects: s.cache.cached_objects as u64,
            cache_hits: s.cache.hits,
            cache_misses: s.cache.misses,
            cache_refills: s.cache.refills,
            cache_drains: s.cache.drains,
            pressure_reclaims: s.cache.pressure_reclaims,
            reclaimed_objects: s.cache.reclaimed_objects,
            cache_cpus: s.cache.enabled_cpus as u64,
            profiling_enabled: cfg!(feature = "heap-profiling") as u64,
            page_allocations: s.buddy.allocation_requests,
            page_failures: s.buddy.allocation_failures,
            page_deallocations: s.buddy.deallocations,
            peak_allocated_pages: s.buddy.peak_allocated_pages as u64,
        }
    }
}
