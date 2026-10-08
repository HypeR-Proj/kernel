// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

use crate::deferred_work::{DeferredWork, WorkDisposition};
use crate::require_ok;

#[test]
fn request_racing_worker_sleep_retains_work_or_elects_a_prompt() {
    loom::model(|| {
        let work = Arc::new(DeferredWork::new());
        let payload = Arc::new(AtomicUsize::new(0));
        assert!(work.claim_initial_worker());
        work.begin_batch();
        let producer = {
            let work = work.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                work.request()
            })
        };

        let disposition = work.finish_batch(false);
        if disposition == WorkDisposition::Continue {
            // Retaining the batch must acquire its newly published work,
            // before joining can supply an unrelated synchronization edge.
            assert_eq!(payload.load(Ordering::Relaxed), 42);
        }
        let prompted = require_ok(producer.join());
        // Both operations have completed; joining is intentional here because
        // this checks ownership, not visibility of the producer's payload.
        match disposition {
            WorkDisposition::Continue => assert!(!prompted),
            WorkDisposition::Wait => {
                assert!(prompted, "work stranded after the worker went to sleep");
                assert!(work.consume_prompt());
                assert!(work.claim_notification());
            }
        }
        work.begin_batch();
        assert_eq!(work.finish_batch(false), WorkDisposition::Wait);
    });
}

#[test]
fn competing_producers_racing_sleep_share_one_prompt_and_can_rearm() {
    loom::model(|| {
        let work = Arc::new(DeferredWork::new());
        assert!(work.claim_initial_worker());
        work.begin_batch();
        let producers: Vec<_> = (0..2)
            .map(|_| {
                let work = work.clone();
                thread::spawn(move || work.request())
            })
            .collect();

        let disposition = work.finish_batch(false);
        let prompts = producers
            .into_iter()
            .map(|producer| require_ok(producer.join()))
            .filter(|prompted| *prompted)
            .count();
        match disposition {
            WorkDisposition::Continue => assert_eq!(prompts, 0),
            WorkDisposition::Wait => {
                assert_eq!(prompts, 1);
                assert!(work.consume_prompt());
                assert!(work.claim_notification());
            }
        }
        assert!(!work.consume_prompt());
        assert!(!work.claim_notification());
        work.begin_batch();
        assert_eq!(work.finish_batch(false), WorkDisposition::Wait);

        // The completed batch must leave neither stale ownership nor a stale
        // prompt that suppresses notification of the next independent request.
        assert!(work.request());
        assert!(work.consume_prompt());
        assert!(work.claim_notification());
        work.begin_batch();
        assert_eq!(work.finish_batch(false), WorkDisposition::Wait);
    });
}

#[test]
fn starting_a_batch_acquires_coalesced_work_or_retains_it_for_the_next_batch() {
    loom::model(|| {
        let work = Arc::new(DeferredWork::new());
        let payload = Arc::new(AtomicUsize::new(0));
        assert!(work.claim_initial_worker());
        work.begin_batch();
        let producer = {
            let work = work.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                // The running worker keeps ownership throughout this race.
                assert!(!work.request());
            })
        };

        work.begin_batch();
        let observed = payload.load(Ordering::Relaxed);
        require_ok(producer.join());
        match work.finish_batch(false) {
            // No work remains: begin_batch consumed the request, and must
            // have acquired its payload before the snapshot above.
            WorkDisposition::Wait => assert_eq!(observed, 42),
            WorkDisposition::Continue => {
                work.begin_batch();
                assert_eq!(work.finish_batch(false), WorkDisposition::Wait);
            }
        }
        assert!(!work.consume_prompt());
        assert!(!work.claim_notification());
    });
}

#[test]
fn irq_notification_acquires_the_producers_payload() {
    loom::model(|| {
        let work = Arc::new(DeferredWork::new());
        let payload = Arc::new(AtomicUsize::new(0));
        let producer = {
            let work = work.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                assert!(work.request());
            })
        };

        // One observation avoids an unbounded polling loop in the model.
        // Exploration includes IRQ service both before and after publication.
        if work.claim_notification() {
            assert_eq!(payload.load(Ordering::Relaxed), 42);
        }
        require_ok(producer.join());
        assert!(work.consume_prompt());
    });
}

#[test]
fn competing_irq_services_elect_one_wakeup_after_deferral() {
    loom::model(|| {
        let work = Arc::new(DeferredWork::new());
        assert!(work.claim_initial_worker());
        work.begin_batch();
        work.defer_until_irq();
        let irq = {
            let work = work.clone();
            thread::spawn(move || work.claim_notification())
        };
        let claimed = work.claim_notification();
        assert_ne!(claimed, require_ok(irq.join()));
        assert!(work.consume_prompt());
        assert!(!work.claim_notification());
        work.begin_batch();
        assert_eq!(work.finish_batch(false), WorkDisposition::Wait);
    });
}
