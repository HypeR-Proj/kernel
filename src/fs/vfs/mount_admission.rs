// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Namespace-use admission independent of topology and cache ownership.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct Admission(AtomicUsize);
const FROZEN: usize = 1 << (usize::BITS - 1);
impl Admission {
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }
    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    pub(super) fn acquire(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < FROZEN - 1).then(|| count + 1)
            })
            .is_ok()
    }
    /// A borrowed live Location proves the count cannot reach zero until this
    /// clone increments it. Thus ordinary freeze cannot race a live clone.
    /// Failed freeze retains that count below the independent admission bit.
    /// Each Location occupies memory, bounding count below the frozen bit.
    pub(super) fn retain(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn release(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
    pub(super) fn freeze(&self) -> bool {
        self.0
            .compare_exchange(0, FROZEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Failed detachment closes new admission while existing owners retain
    /// their count. Their clones and drops cannot clear the frozen bit.
    pub(super) fn freeze_failed(&self) -> bool {
        self.0.fetch_or(FROZEN, Ordering::AcqRel) & FROZEN == 0
    }
    /// Only the owner of a successful freeze can roll back failed unmount.
    pub(super) fn thaw(&self) {
        self.0.fetch_and(!FROZEN, Ordering::Release);
    }
}
