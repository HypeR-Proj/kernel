// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the production reader lanes with real concurrent host threads.

use hyper::hal::interrupt::InterruptMask;
use hyper::sync::InterruptShardedLock;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

thread_local! {
    static WAITING: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
}

struct Mask;
impl InterruptMask for Mask {
    type State = ();
    fn save_and_disable() {}
    fn restore(_: ()) {}
    fn wait_for_lock_owner() {
        WAITING.with(|waiting| {
            if let Some(sent) = waiting.borrow_mut().take() {
                let _ = sent.send(());
            }
        });
        thread::yield_now();
    }
}

#[test]
fn independent_readers_overlap() {
    let lock = InterruptShardedLock::<_, Mask, 4>::new(41usize);
    let (entered, arrivals) = mpsc::channel();
    thread::scope(|scope| {
        let mut releases = Vec::new();
        for lane in 0..4 {
            let entered = entered.clone();
            let lock = &lock;
            let (release, released) = mpsc::channel();
            releases.push(release);
            scope.spawn(move || {
                assert_eq!(
                    lock.read(lane, |value| {
                        assert_eq!(*value, 41);
                        assert!(entered.send(()).is_ok());
                        // A failed overlap assertion must not strand scoped workers.
                        let _ = released.recv_timeout(Duration::from_secs(10));
                    }),
                    Some(())
                );
            });
        }
        let overlap = (0..4).all(|_| arrivals.recv_timeout(Duration::from_secs(5)).is_ok());
        for release in releases {
            let _ = release.send(());
        }
        assert!(
            overlap,
            "all reader lanes must enter before any is released"
        );
    });
    assert_eq!(lock.read(4, |value| *value), None);
}

#[test]
fn writer_waits_for_each_lane() {
    for lane in 0..4 {
        let lock = InterruptShardedLock::<_, Mask, 4>::new(());
        let committed = AtomicBool::new(false);
        let (waiting, observed) = mpsc::channel();
        thread::scope(|scope| {
            // Hold only this lane: acquiring a subset of lanes must fail the test.
            let blocked = lock.read(lane, |_| {
                scope.spawn(|| {
                    WAITING.with(|slot| *slot.borrow_mut() = Some(waiting));
                    lock.with(|_| committed.store(true, Ordering::Release));
                });
                // Observe actual lock contention instead of assuming the writer
                // was scheduled during an arbitrary sleep interval.
                let blocked = observed.recv_timeout(Duration::from_secs(5)).is_ok();
                blocked && !committed.load(Ordering::Acquire)
            });
            // The reader has released its lane even if this assertion fails.
            assert_eq!(blocked, Some(true), "writer did not wait for lane {lane}");
        });
        assert!(committed.load(Ordering::Acquire));
    }
}

#[test]
fn writer_publication_and_lane_reuse_remain_exclusive_under_contention() {
    let lock = InterruptShardedLock::<_, Mask, 4>::new((0usize, 0usize));
    let active = AtomicUsize::new(0);
    thread::scope(|scope| {
        for worker in 0..8 {
            let lock = &lock;
            let active = &active;
            scope.spawn(move || {
                for _ in 0..1000 {
                    if worker < 2 {
                        lock.with(|value| {
                            assert_eq!(active.load(Ordering::SeqCst), 0);
                            value.0 += 1;
                            thread::yield_now();
                            value.1 = value.0;
                        });
                    } else {
                        assert_eq!(
                            lock.read(worker % 4, |value| {
                                active.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(value.0, value.1);
                                thread::yield_now();
                                assert_eq!(value.0, value.1);
                                active.fetch_sub(1, Ordering::SeqCst);
                            }),
                            Some(())
                        );
                    }
                }
            });
        }
    });
    assert_eq!(lock.read(0, |value| *value), Some((2000, 2000)));
}

#[test]
fn unwind_releases_read_and_write_lanes() {
    let lock = InterruptShardedLock::<_, Mask, 3>::new(0usize);
    let reader = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lock.read(2, |_| panic!("reader test unwind"));
    }));
    assert!(reader.is_err());
    let writer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lock.with(|_| panic!("writer test unwind"));
    }));
    assert!(writer.is_err());
    lock.with(|value| *value = 7);
    assert_eq!(lock.read(1, |value| *value), Some(7));
}

#[test]
fn try_writer_releases_partial_lanes_without_waiting() {
    for lane in 0..4 {
        let lock = InterruptShardedLock::<_, Mask, 4>::new(0usize);
        let (attempted, outcome) = mpsc::channel();
        thread::scope(|scope| {
            let observed = lock.read(lane, |_| {
                let lock = &lock;
                scope.spawn(move || {
                    let result = lock.try_with(|value| *value = 1);
                    let _ = attempted.send(result);
                });
                // Keep the contended lane held until the attempt returns.
                // On failure, release it before joining the scoped worker.
                outcome.recv_timeout(Duration::from_secs(5))
            });
            assert_eq!(observed, Some(Ok(None)));
        });
        // A failure at lane N must release every earlier lane as well.
        assert_eq!(lock.try_with(|value| *value), Some(0));
        assert_eq!(lock.try_with(|value| *value = 2), Some(()));
        assert_eq!(lock.read(lane, |value| *value), Some(2));
    }
}

#[test]
fn failed_try_writer_restores_interrupt_state_and_unwind_releases_lanes() {
    thread_local! {
        static MASKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    struct TrackedMask;
    impl InterruptMask for TrackedMask {
        type State = bool;
        fn save_and_disable() -> bool {
            MASKED.with(|masked| masked.replace(true))
        }
        fn restore(state: bool) {
            MASKED.with(|masked| masked.set(state));
        }
    }
    let lock = InterruptShardedLock::<_, TrackedMask, 3>::new(0usize);
    let (attempted, outcome) = mpsc::channel();
    thread::scope(|scope| {
        let observed = lock.read(2, |_| {
            let lock = &lock;
            scope.spawn(move || {
                let failed = lock.try_with(|_| panic!("contended writer entered"));
                let restored = !MASKED.with(|masked| masked.get());
                let _ = attempted.send((failed, restored));
            });
            outcome.recv_timeout(Duration::from_secs(5))
        });
        assert_eq!(observed, Some(Ok((None, true))));
    });
    assert!(!MASKED.with(|masked| masked.get()));
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lock.try_with(|_| {
            assert!(MASKED.with(|masked| masked.get()));
            panic!("try writer test unwind");
        });
    }));
    assert!(unwind.is_err());
    assert!(!MASKED.with(|masked| masked.get()));
    MASKED.with(|masked| masked.set(true));
    assert_eq!(lock.try_with(|value| *value = 3), Some(()));
    assert!(MASKED.with(|masked| masked.get()));
    MASKED.with(|masked| masked.set(false));
    assert_eq!(lock.read(0, |value| *value), Some(3));
}
