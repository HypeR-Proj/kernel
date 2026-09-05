// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host checks for allocation-free reschedule request publication.

#[path = "../../../../src/kernel/task/reschedule.rs"]
mod request;

use request::PendingReschedule;

use hyper::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Barrier;

#[test]
fn reschedule_requests_coalesce_and_survive_a_completed_take() {
    let pending = PendingReschedule::new();
    assert!(!pending.is_pending());

    assert!(pending.publish());
    assert!(!pending.publish());
    assert!(pending.is_pending());
    assert!(pending.take());
    assert!(!pending.take());

    assert!(pending.publish());
    assert!(pending.take());
}

#[test]
fn concurrent_publishers_elect_exactly_one_notifier_per_epoch() {
    const PUBLISHERS: usize = 8;
    const EPOCHS: usize = 16;

    let pending = PendingReschedule::new();
    for _ in 0..EPOCHS {
        let barrier = Barrier::new(PUBLISHERS + 1);
        let elected = std::thread::scope(|scope| {
            let mut publishers = Vec::with_capacity(PUBLISHERS);
            for _ in 0..PUBLISHERS {
                publishers.push(scope.spawn(|| {
                    barrier.wait();
                    pending.publish()
                }));
            }
            barrier.wait();
            publishers
                .into_iter()
                .map(|publisher| match publisher.join() {
                    Ok(elected) => elected,
                    Err(_) => panic!("reschedule publisher panicked"),
                })
                .filter(|elected| *elected)
                .count()
        });

        assert_eq!(elected, 1);
        assert!(pending.take());
        assert!(!pending.is_pending());
    }
}

#[test]
fn acquire_observation_sees_state_published_before_the_request() {
    const ITERATIONS: usize = 64;

    for value in 1..=ITERATIONS {
        let pending = PendingReschedule::new();
        let queue_state = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                queue_state.store(value, Ordering::Relaxed);
                assert!(pending.publish());
            });
            while !pending.is_pending() {
                core::hint::spin_loop();
            }
            assert_eq!(queue_state.load(Ordering::Relaxed), value);
        });
    }
}

#[test]
fn consuming_take_acquires_state_when_it_is_the_first_observer() {
    const ITERATIONS: usize = 64;

    for value in 1..=ITERATIONS {
        let pending = PendingReschedule::new();
        let queue_state = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                queue_state.store(value, Ordering::Relaxed);
                assert!(pending.publish());
            });
            while !pending.take() {
                core::hint::spin_loop();
            }
            assert_eq!(queue_state.load(Ordering::Relaxed), value);
        });
    }
}
