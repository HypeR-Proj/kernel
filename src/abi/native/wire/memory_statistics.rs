// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Typed diagnostic records are encoded without exposing Rust padding.
use hyper::abi::native::*;
use {crate::diagnostics::inspect::memory::Data, crate::diagnostics::inspect::memory::Statistics};

pub(in crate::abi::native) const MAX_RECORD: usize =
    core::mem::size_of::<HyperNativeMemoryHeapStatistics>();
pub(in crate::abi::native) fn encode(snapshot: Statistics, bytes: &mut [u8; MAX_RECORD]) -> usize {
    macro_rules! field {
        ($record:ty, $name:ident, $value:expr) => {
            super::write_u64(bytes, core::mem::offset_of!($record, $name), $value as u64);
        };
    }
    match snapshot.data {
        Data::Heap(s) => {
            type Record = HyperNativeMemoryHeapStatistics;
            field!(Record, captured_at_ns, snapshot.captured_at_ns);
            field!(Record, live_objects, s.live_objects);
            field!(Record, live_slab_objects, s.live_slab_objects);
            field!(Record, live_large_objects, s.live_large_objects);
            field!(Record, requested_bytes, s.requested_bytes);
            field!(Record, peak_objects, s.peak_objects);
            field!(Record, peak_requested_bytes, s.peak_requested_bytes);
            field!(Record, allocations, s.allocations);
            field!(Record, failures, s.failures);
            field!(Record, cached_objects, s.cached_objects);
            field!(Record, cache_hits, s.cache_hits);
            field!(Record, cache_misses, s.cache_misses);
            field!(Record, cache_refills, s.cache_refills);
            field!(Record, cache_drains, s.cache_drains);
            field!(Record, pressure_reclaims, s.pressure_reclaims);
            field!(Record, reclaimed_objects, s.reclaimed_objects);
            field!(Record, cache_cpus, s.cache_cpus);
            field!(Record, profiling_enabled, s.profiling_enabled);
            field!(Record, page_allocations, s.page_allocations);
            field!(Record, page_failures, s.page_failures);
            field!(Record, page_deallocations, s.page_deallocations);
            field!(Record, peak_allocated_pages, s.peak_allocated_pages);
            core::mem::size_of::<Record>()
        }
        Data::Buddy { order, free_blocks } => {
            type Record = HyperNativeMemoryBuddyStatistics;
            field!(Record, captured_at_ns, snapshot.captured_at_ns);
            field!(Record, order, order);
            field!(Record, free_blocks, free_blocks);
            core::mem::size_of::<Record>()
        }
        Data::Owner(s) => {
            type Record = HyperNativeMemoryOwnerStatistics;
            field!(Record, captured_at_ns, snapshot.captured_at_ns);
            field!(Record, resident_pages, s.pages);
            field!(Record, peak_pages, s.peak_pages);
            field!(Record, allocations, s.allocation_requests);
            field!(Record, failures, s.allocation_failures);
            field!(Record, deallocations, s.deallocations);
            core::mem::size_of::<Record>()
        }
        Data::FileCache {
            usage,
            payload_pages,
            slab_pages,
            large_pages,
        } => {
            type Record = HyperNativeMemoryFileCacheStatistics;
            field!(Record, captured_at_ns, snapshot.captured_at_ns);
            field!(Record, payload_pages, payload_pages);
            field!(Record, metadata_slab_pages, slab_pages);
            field!(Record, metadata_large_pages, large_pages);
            field!(Record, index_capacity, usage.capacity);
            field!(Record, clean_entries, usage.clean);
            field!(Record, loading_entries, usage.loading);
            field!(Record, live_payloads, usage.live_payloads);
            field!(Record, metadata_budget_pages, usage.metadata_pages);
            field!(Record, table_present, usage.table_present);
            field!(Record, admission_enabled, usage.admission_enabled);
            field!(Record, admission_paused, usage.admission_paused);
            field!(Record, maintenance_pending, usage.maintenance_pending);
            field!(Record, maintenance_running, usage.maintenance_running);
            field!(Record, growth_requested, usage.growth_requested);
            core::mem::size_of::<Record>()
        }
        Data::HeapClass(s) => {
            type Record = HyperNativeMemoryHeapClassStatistics;
            field!(Record, captured_at_ns, snapshot.captured_at_ns);
            field!(Record, pool, s.pool);
            field!(Record, object_size, s.object_size);
            field!(Record, pages, s.pages);
            field!(Record, capacity, s.capacity);
            field!(Record, reserved, s.reserved);
            field!(Record, cached, s.cached);
            field!(Record, live, s.live);
            field!(Record, requested_bytes, s.requested_bytes);
            field!(Record, allocations, s.allocations);
            field!(Record, deallocations, s.deallocations);
            core::mem::size_of::<Record>()
        }
    }
}
