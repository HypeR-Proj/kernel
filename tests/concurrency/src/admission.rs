// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

use crate::require_ok;
use crate::run_admission::{AdmissionError, RunAdmission};

#[test]
fn close_counts_a_racing_admission_or_rejects_it() {
    loom::model(|| {
        let gate = Arc::new(RunAdmission::new(7));
        let runner = {
            let gate = gate.clone();
            // Return the live claim; release cannot race the count assertion.
            thread::spawn(move || gate.admit())
        };
        let active_at_close = gate.close();
        let claim = require_ok(runner.join());
        assert_eq!(active_at_close, usize::from(claim.is_ok()));
        assert_eq!(gate.active_count(), active_at_close);
        assert_eq!(gate.is_closed_and_quiescent(), claim.is_err());
        assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
        match claim {
            Ok(claim) => gate.release(claim),
            Err(error) => assert_eq!(error, AdmissionError::Closed),
        }
        assert!(gate.is_closed_and_quiescent());
    });
}

#[test]
fn quiescence_acquires_every_runners_final_writes() {
    loom::model(|| {
        let gate = Arc::new(RunAdmission::new(7));
        let first = require_ok(gate.admit());
        let second = require_ok(gate.admit());
        let payload = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let runners = [first, second]
            .into_iter()
            .enumerate()
            .map(|(index, claim)| {
                let gate = gate.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    payload[index].store(index + 1, Ordering::Relaxed);
                    gate.release(claim);
                })
            });
        let runners: Vec<_> = runners.collect();

        gate.close();
        if gate.is_closed_and_quiescent() {
            // No join or extra acquire before these reads. The admission
            // state must publish both runners, regardless of release order.
            assert_eq!(payload[0].load(Ordering::Relaxed), 1);
            assert_eq!(payload[1].load(Ordering::Relaxed), 2);
        }
        for runner in runners {
            require_ok(runner.join());
        }
        assert!(gate.is_closed_and_quiescent());
    });
}

#[test]
fn close_counts_every_competing_admission_without_losing_an_increment() {
    loom::model(|| {
        let gate = Arc::new(RunAdmission::new(7));
        let runners: Vec<_> = (0..2)
            .map(|_| {
                let gate = gate.clone();
                thread::spawn(move || gate.admit())
            })
            .collect();

        let active_at_close = gate.close();
        // Keep every successful claim alive until both attempts have resolved.
        // This makes the close count exact, without assuming their race order.
        let claims: Vec<_> = runners
            .into_iter()
            .map(|runner| require_ok(runner.join()))
            .collect();
        let admitted = claims.iter().filter(|claim| claim.is_ok()).count();
        assert_eq!(active_at_close, admitted);
        assert_eq!(gate.active_count(), admitted);
        assert_eq!(gate.is_closed_and_quiescent(), admitted == 0);
        assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
        for claim in claims {
            match claim {
                Ok(claim) => gate.release(claim),
                Err(error) => assert_eq!(error, AdmissionError::Closed),
            }
        }
        assert!(gate.is_closed_and_quiescent());
    });
}

#[test]
fn competing_closers_preserve_live_claims_until_the_last_release() {
    loom::model(|| {
        let gate = Arc::new(RunAdmission::new(7));
        let first = require_ok(gate.admit());
        let second = require_ok(gate.admit());
        let closer = {
            let gate = gate.clone();
            thread::spawn(move || gate.close())
        };
        assert_eq!(gate.close(), 2);
        assert_eq!(require_ok(closer.join()), 2);
        assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
        assert!(!gate.is_closed_and_quiescent());

        gate.release(first);
        assert_eq!(gate.active_count(), 1);
        assert!(!gate.is_closed_and_quiescent());
        gate.release(second);
        assert!(gate.is_closed_and_quiescent());
        assert_eq!(gate.close(), 0);
        assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
    });
}
