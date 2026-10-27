// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free publication of coalesced task reschedule requests.

#[cfg(not(loom))]
use hyper::sync::atomic::{AtomicBool, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};

/// One per-CPU coalesced reschedule notification.
pub(crate) struct PendingReschedule(AtomicBool);

impl PendingReschedule {
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    #[cfg(loom)]
    pub fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Publishes queue changes performed before this call to the target CPU.
    ///
    /// Returns `true` only for the publisher which transitions the request
    /// from clear to pending. That publisher owns notification of the target;
    /// later publishers coalesce behind the same pending request.
    pub fn publish(&self) -> bool {
        // Use an RMW so concurrent publishers form one release sequence. The
        // scheduler lock remains the authority for queue-state visibility;
        // this atomic preserves the notification edge and elects exactly one
        // notifier for each clear-to-pending interval.
        !self.0.swap(true, Ordering::Release)
    }

    /// Observes whether at least one request has been published.
    pub fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Consumes requests while the caller serializes the scheduling decision.
    ///
    /// Requests published after this exchange remain pending. The Acquire
    /// half observes the release sequence formed by concurrent publishers.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}
