// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

use crate::require_ok;
use crate::reschedule::PendingReschedule;

#[test]
fn taking_a_racing_request_acquires_payload_or_leaves_it_pending() {
    check_publication(true);
}

#[test]
fn observing_a_pending_request_acquires_payload() {
    check_publication(false);
}

fn check_publication(consume: bool) {
    loom::model(move || {
        let pending = Arc::new(PendingReschedule::new());
        let payload = Arc::new(AtomicUsize::new(0));
        let publisher = {
            let pending = pending.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                assert!(pending.publish());
            })
        };

        let observed = if consume {
            pending.take()
        } else {
            pending.is_pending()
        };
        if observed {
            assert_eq!(payload.load(Ordering::Relaxed), 42);
        }
        require_ok(publisher.join());
        assert_eq!(pending.take(), !consume || !observed);
        assert!(!pending.is_pending());
    });
}

#[test]
fn competing_publishers_elect_one_notifier_in_each_reused_epoch() {
    loom::model(|| {
        let pending = Arc::new(PendingReschedule::new());
        for _ in 0..2 {
            let publisher = {
                let pending = pending.clone();
                thread::spawn(move || pending.publish())
            };
            let elected = pending.publish();
            // Joining is deliberate: no consumer may split this epoch while
            // the two publishers compete for its single notification.
            assert_ne!(elected, require_ok(publisher.join()));
            assert!(pending.take());
            assert!(!pending.take());
        }
    });
}

#[test]
fn coalesced_publication_is_acquired_or_starts_a_new_epoch() {
    loom::model(|| {
        let pending = Arc::new(PendingReschedule::new());
        let payload = Arc::new(AtomicUsize::new(0));
        assert!(pending.publish());
        let publisher = {
            let pending = pending.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                pending.publish()
            })
        };

        assert!(pending.take());
        let observed = payload.load(Ordering::Relaxed);
        let elected = require_ok(publisher.join());
        // A coalesced publisher preceded take, so even a request that elected
        // no notification must publish its payload. Inspect the pre-join value.
        if !elected {
            assert_eq!(observed, 42);
        }
        // A later publication instead owns a fresh notification and must not
        // have been erased by consumption of the preceding epoch.
        assert_eq!(pending.take(), elected);
        assert!(!pending.is_pending());
    });
}
