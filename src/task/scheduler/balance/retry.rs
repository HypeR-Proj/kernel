// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A serialized optional writer supplies the destination for lock notifications.

#[cfg(not(loom))]
use hyper::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct ReleaseRetry {
    target: AtomicUsize,
}

impl ReleaseRetry {
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self {
            target: AtomicUsize::new(0),
        }
    }

    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self {
            target: AtomicUsize::new(0),
        }
    }

    /// Register before `InterruptShardedLock::try_with` publishes NOTIFY in
    /// its blocking lane. That lane's release acquires this publication.
    pub(super) fn register(&self, cpu: usize) {
        self.target.store(cpu + 1, Ordering::Release);
    }

    /// Called inside a successfully acquired coordinator transaction.
    pub(super) fn acquired(&self) {
        self.target.store(0, Ordering::Release);
    }

    /// Only the sharded lock's registered release callback may consume this.
    /// An unrelated load after unlocking would not close the registration race.
    pub(super) fn released(&self) -> Option<usize> {
        self.target.swap(0, Ordering::AcqRel).checked_sub(1)
    }
}
