// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Namespace-use admission independent of topology and cache ownership.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct Admission(AtomicUsize);
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
                (count < usize::MAX - 1).then(|| count + 1)
            })
            .is_ok()
    }
    /// A borrowed live Location proves the count cannot reach zero until this
    /// clone increments it. Thus freeze cannot succeed concurrently with clone.
    /// Each Location occupies memory, bounding count far below `usize::MAX`.
    pub(super) fn retain(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn release(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
    pub(super) fn freeze(&self) -> bool {
        self.0
            .compare_exchange(0, usize::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Only the owner of a successful freeze can roll back failed unmount.
    pub(super) fn thaw(&self) {
        self.0.store(0, Ordering::Release);
    }
}
