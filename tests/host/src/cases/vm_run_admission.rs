// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/vm/run_admission.rs"]
mod admission_model;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use admission_model::{AdmissionError, RunAdmission};

#[test]
fn admission_and_close_have_one_packed_linearization_point() {
    for iteration in 0..128_u64 {
        let gate = Arc::new(RunAdmission::new(iteration));
        let start = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let outcome = Arc::new(AtomicUsize::new(0));
        let worker_gate = gate.clone();
        let worker_start = start.clone();
        let worker_release = release.clone();
        let worker_outcome = outcome.clone();
        let worker = std::thread::spawn(move || {
            worker_start.wait();
            let claim = worker_gate.admit();
            worker_outcome.store(if claim.is_ok() { 1 } else { 2 }, Ordering::Release);
            worker_release.wait();
            match claim {
                Ok(claim) => worker_gate.release(claim),
                Err(error) => assert_eq!(error, AdmissionError::Closed),
            }
        });

        start.wait();
        let active_at_close = gate.close();
        while outcome.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        let admitted = outcome.load(Ordering::Relaxed) == 1;
        assert_eq!(active_at_close, usize::from(admitted));
        release.wait();
        assert!(worker.join().is_ok());
        assert!(gate.is_closed_and_quiescent());
        assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
    }
}

#[test]
fn final_claim_release_establishes_closed_quiescence() {
    let gate = RunAdmission::new(7);
    let first = crate::require_ok(gate.admit());
    let second = crate::require_ok(gate.admit());
    assert_eq!(gate.active_count(), 2);
    assert_eq!(gate.close(), 2);
    assert!(!gate.is_closed_and_quiescent());

    gate.release(first);
    assert_eq!(gate.active_count(), 1);
    assert!(!gate.is_closed_and_quiescent());
    gate.release(second);
    assert_eq!(gate.active_count(), 0);
    assert!(gate.is_closed_and_quiescent());
}

#[test]
fn lookup_lease_survives_registry_unpublication() {
    struct CountedDrop(Arc<AtomicUsize>);

    impl Drop for CountedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let mut slot = Some(owner);
    let lease = match slot.as_ref() {
        Some(owner) => owner.clone(),
        None => return,
    };
    drop(slot.take());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(lease);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn scoped_lease_blocks_unique_retirement_until_drop() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(41_u64));
    let lease = owner.clone();
    let conversion = owner.try_into_unique();
    assert!(conversion.is_err());
    let owner = match conversion {
        Ok(_) => return,
        Err(owner) => owner,
    };
    drop(lease);
    let conversion = owner.try_into_unique();
    assert!(conversion.is_ok());
    let unique = match conversion {
        Ok(unique) => unique,
        Err(_) => return,
    };
    assert_eq!(unique.into_inner(), 41);
}
