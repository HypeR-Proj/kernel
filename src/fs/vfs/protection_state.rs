// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Nonblocking retirement of content protection admitted under the file gate.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct ProtectionState(AtomicUsize);

impl ProtectionState {
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    /// The caller holds the file's content gate, shared with every writer.
    pub(super) fn acquire(&self) -> bool {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            })
            .is_ok()
    }

    /// Last-owner destruction may run on any CPU, outside the file gate.
    pub(super) fn release(&self) {
        let previous = self.0.fetch_sub(1, Ordering::Release);
        assert!(previous != 0, "file protection ownership underflow");
    }

    /// Hold the content gate from this check until the mutation completes.
    /// The gate serializes acquisition; Acquire observes final guard release.
    pub(super) fn writable(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}
