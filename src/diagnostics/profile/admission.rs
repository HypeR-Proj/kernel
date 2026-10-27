// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! The requested generation and admitted IRQ writers share one atomic word.
//! Closing and claiming have one linearization point: stop cannot recycle
//! sample buffers until every admitted writer has retired. Per-CPU owners are
//! only local bookkeeping, never the authority for reclaiming a capture.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};

const COUNT_BITS: u32 = 7;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;
pub(super) const MAX_GENERATION: u64 = u64::MAX >> COUNT_BITS;

pub(super) struct Admission<const N: usize> {
    state: AtomicU64,
    owners: [AtomicU64; N],
}
impl<const N: usize> Admission<N> {
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            owners: [const { AtomicU64::new(0) }; N],
        }
    }
    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            owners: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    pub(super) fn requested(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> COUNT_BITS
    }
    /// Serialized control only: the previous request is closed and quiescent;
    /// buffers/configuration are initialized; 1 <= generation <= `MAX_GENERATION`.
    pub(super) fn publish(&self, generation: u64) {
        self.state
            .store(generation << COUNT_BITS, Ordering::Release);
    }
    pub(super) fn close(&self, generation: u64) {
        let mut state = self.state.load(Ordering::Acquire);
        while state >> COUNT_BITS == generation && generation != 0 {
            match self.state.compare_exchange_weak(
                state,
                state & COUNT_MASK,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }
    /// Each CPU claims at most once and owns its slot with local IRQs masked.
    pub(super) fn claim(&self, cpu: usize, generation: u64) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        while generation != 0
            && state >> COUNT_BITS == generation
            && state & COUNT_MASK < COUNT_MASK
        {
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.owners[cpu].store(generation, Ordering::Relaxed);
                    return true;
                }
                Err(observed) => state = observed,
            }
        }
        false
    }
    pub(super) fn owner(&self, cpu: usize) -> u64 {
        self.owners[cpu].load(Ordering::Relaxed)
    }
    /// All sampling and PMU writes finish before acknowledging stop. The RMW
    /// acquires preceding releases so the final acquire observes every writer.
    pub(super) fn release(&self, cpu: usize) {
        self.owners[cpu].store(0, Ordering::Relaxed);
        self.state.fetch_sub(1, Ordering::AcqRel);
    }
    pub(super) fn active(&self) -> u64 {
        self.state.load(Ordering::Acquire) & COUNT_MASK
    }
    pub(super) fn quiescent(&self) -> bool {
        self.active() == 0
    }
}
