// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional, bounded allocation traffic and occupancy counters. No addresses,
//! allocation records, stack walks, or dynamically allocated bookkeeping.

#[cfg(feature = "heap-profiling")]
use crate::sync::atomic::{AtomicU64, Ordering};

/// Eight slab classes and one large-allocation bucket in each heap pool.
pub const BUCKETS: usize = 2 * (super::SLAB_CLASS_COUNT + 1);

/// Independently sampled logical counters and lock-consistent backing counts.
/// In-flight magazine transfers are reserved slots, not live caller objects.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeapClassStats {
    pub pool: usize,
    /// Zero identifies large allocations, otherwise the slab slot size.
    pub object_size: usize,
    pub pages: usize,
    pub capacity: usize,
    pub reserved: usize,
    pub cached: usize,
    pub live: u64,
    pub requested_bytes: u64,
    pub allocations: u64,
    pub deallocations: u64,
}

#[cfg(feature = "heap-profiling")]
#[derive(Clone, Copy)]
pub(super) struct Backing {
    pub(super) pages: usize,
    pub(super) capacity: usize,
    pub(super) reserved: usize,
}

#[cfg(feature = "heap-profiling")]
impl Backing {
    pub(super) const EMPTY: Self = Self {
        pages: 0,
        capacity: 0,
        reserved: 0,
    };
}

#[cfg(feature = "heap-profiling")]
pub(super) fn bucket(pool: super::HeapPool, class: Option<super::HeapSlabClass>) -> usize {
    pool as usize * (super::SLAB_CLASS_COUNT + 1)
        + class.map_or(super::SLAB_CLASS_COUNT, |class| class.index())
}

#[cfg(feature = "heap-profiling")]
pub(super) struct Traffic {
    live: AtomicU64,
    bytes: AtomicU64,
    allocations: AtomicU64,
    deallocations: AtomicU64,
}

#[cfg(feature = "heap-profiling")]
impl Traffic {
    pub(super) const fn new() -> Self {
        Self {
            live: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            allocations: AtomicU64::new(0),
            deallocations: AtomicU64::new(0),
        }
    }

    pub(super) fn allocate(&self, size: usize) {
        // The allocator records this before returning the pointer. Ownership
        // transfer, not these diagnostic atomics, orders a remote free.
        self.live.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(size.max(1) as u64, Ordering::Relaxed);
        increment(&self.allocations);
    }

    pub(super) fn deallocate(&self, size: usize) {
        self.live.fetch_sub(1, Ordering::Relaxed);
        self.bytes.fetch_sub(size.max(1) as u64, Ordering::Relaxed);
        increment(&self.deallocations);
    }

    pub(super) fn snapshot(&self, row: &mut HeapClassStats) {
        row.live = self.live.load(Ordering::Relaxed);
        row.requested_bytes = self.bytes.load(Ordering::Relaxed);
        row.allocations = self.allocations.load(Ordering::Relaxed);
        row.deallocations = self.deallocations.load(Ordering::Relaxed);
    }
}

#[cfg(feature = "heap-profiling")]
fn increment(value: &AtomicU64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(1))
    });
}
