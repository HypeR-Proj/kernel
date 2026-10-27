// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free, generation-qualified CPU-local comparator membership.

use hyper::hal::timer::deadline_reached;
use hyper::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

/// Single-writer publication from a CPU scheduler lock to its local timer.
///
/// The scheduler serializes writers, including remote writers, with the target
/// CPU's scheduler lock. The consumer never acquires that lock. Every payload
/// word is atomic, so an overlapping read is safe and retries if its sequence
/// changed. The dirty bit is cleared before reading: a later writer retains a
/// new prompt even when this read already observes its payload.
pub(super) struct DeadlineMailbox {
    sequence: AtomicU64,
    generation: AtomicU64,
    deadline: AtomicU64,
    present: AtomicBool,
    dirty: AtomicBool,
}

impl DeadlineMailbox {
    pub(super) const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            present: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
        }
    }

    /// Caller serializes all writers and must not be interrupted by a reader
    /// on the same CPU. Returns whether a target prompt must be sent, including
    /// a local publication: ordinary kernel execution need not reschedule soon.
    pub(super) fn publish(&self, generation: u64, deadline: Option<u64>) -> bool {
        if generation <= self.generation.load(Ordering::Relaxed) {
            return false;
        }
        self.sequence.fetch_add(1, Ordering::Relaxed);
        // Pair with the reader's Acquire fence through any payload atomic it
        // observes from this publication. Seeing a new payload word then
        // requires the second sequence load to observe this odd version (or
        // a later one), rather than accepting a mixed snapshot as the old run.
        fence(Ordering::Release);
        self.generation.store(generation, Ordering::Relaxed);
        self.deadline
            .store(deadline.unwrap_or(0), Ordering::Relaxed);
        self.present.store(deadline.is_some(), Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
        !self.dirty.swap(true, Ordering::AcqRel)
    }

    pub(super) fn take(&self) -> Option<(u64, Option<u64>)> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let generation = self.generation.load(Ordering::Relaxed);
            let deadline = self.deadline.load(Ordering::Relaxed);
            let present = self.present.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == sequence {
                return Some((generation, present.then_some(deadline)));
            }
        }
    }
}

/// A permanent producer's latest deadline and deferred expiry notification.
///
/// Producers serialize generation allocation in their own state lock, then
/// publish here after releasing that lock. The timer lock serializes this
/// slot. Remembering consumed generations prevents an out-of-order or repeated
/// publication from resurrecting an expired deadline.
pub(super) struct DeadlineSlot {
    generation: u64,
    deadline: Option<u64>,
    ready: Option<u64>,
}

impl DeadlineSlot {
    pub(super) const fn new() -> Self {
        Self {
            generation: 0,
            deadline: None,
            ready: None,
        }
    }

    pub(super) fn publish(&mut self, generation: u64, deadline: Option<u64>) -> bool {
        if generation <= self.generation() {
            return false;
        }
        self.generation = generation;
        self.deadline = deadline;
        self.ready = None;
        true
    }

    pub(super) const fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn expire(&mut self, now: u64) -> bool {
        if self
            .deadline
            .is_some_and(|deadline| deadline_reached(now, deadline))
        {
            self.deadline = None;
            self.ready = Some(self.generation);
            true
        } else {
            false
        }
    }

    pub(super) fn take_ready(&mut self) -> Option<u64> {
        self.ready.take()
    }
}

/// Both deadlines must lie within half a counter period of each other.
pub(super) fn earlier(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if deadline_reached(left, right) {
            right
        } else {
            left
        }),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{DeadlineMailbox, DeadlineSlot, earlier};

    #[test]
    fn mailbox_coalesces_prompts_and_keeps_cancellation() {
        let mailbox = DeadlineMailbox::new();
        assert!(mailbox.publish(1, Some(100)));
        assert!(!mailbox.publish(2, Some(50)));
        assert_eq!(mailbox.take(), Some((2, Some(50))));
        assert_eq!(mailbox.take(), None);
        assert!(mailbox.publish(3, None));
        assert!(!mailbox.publish(2, Some(10)));
        assert_eq!(mailbox.take(), Some((3, None)));
        assert!(mailbox.publish(4, Some(0)));
        assert_eq!(mailbox.take(), Some((4, Some(0))));
    }

    #[test]
    fn mailbox_never_pairs_a_generation_with_another_publications_deadline() {
        use hyper::sync::atomic::{AtomicBool, Ordering};

        let mailbox = DeadlineMailbox::new();
        let complete = AtomicBool::new(false);
        const LAST: u64 = 100_000;
        fn deadline(generation: u64) -> Option<u64> {
            (!generation.is_multiple_of(5)).then_some(generation * 17)
        }
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for generation in 1..=LAST {
                    mailbox.publish(generation, deadline(generation));
                }
                complete.store(true, Ordering::Release);
            });
            let mut last = 0;
            loop {
                if let Some((generation, observed)) = mailbox.take() {
                    assert!(generation >= last);
                    assert_eq!(observed, deadline(generation));
                    last = generation;
                } else if complete.load(Ordering::Acquire) {
                    // The final dirty publication precedes complete. Recheck
                    // once after observing complete to close the empty race.
                    if let Some((generation, observed)) = mailbox.take() {
                        assert!(generation >= last);
                        assert_eq!(observed, deadline(generation));
                        last = generation;
                    }
                    assert_eq!(last, LAST);
                    break;
                }
            }
        });
    }

    #[test]
    fn stale_publish_cannot_override_cancel_or_rearm_expired_generation() {
        let mut slot = DeadlineSlot::new();
        assert!(slot.publish(2, Some(200)));
        assert!(!slot.publish(1, Some(100)));
        slot.expire(200);
        assert_eq!(slot.deadline(), None);
        assert_eq!(slot.take_ready(), Some(2));
        assert!(!slot.publish(2, Some(200)));
        assert!(slot.publish(4, None));
        assert!(!slot.publish(3, Some(250)));
        slot.expire(1_000);
        assert_eq!(slot.take_ready(), None);
    }

    #[test]
    fn replacing_an_expiry_before_delivery_retires_the_old_notification() {
        let mut slot = DeadlineSlot::new();
        assert!(slot.publish(1, Some(10)));
        slot.expire(10);
        assert!(slot.publish(2, Some(20)));
        assert_eq!(slot.take_ready(), None);
        slot.expire(19);
        assert_eq!(slot.take_ready(), None);
        slot.expire(20);
        assert_eq!(slot.take_ready(), Some(2));
        assert_eq!(slot.take_ready(), None);
    }

    #[test]
    fn deadline_order_and_expiry_cross_counter_wrap() {
        assert_eq!(earlier(Some(u64::MAX - 1), Some(3)), Some(u64::MAX - 1));
        assert_eq!(earlier(None, Some(3)), Some(3));
        assert_eq!(earlier(None, None), None);
        let mut slot = DeadlineSlot::new();
        assert!(slot.publish(1, Some(3)));
        slot.expire(u64::MAX);
        assert_eq!(slot.take_ready(), None);
        slot.expire(3);
        assert_eq!(slot.take_ready(), Some(1));
    }
}
