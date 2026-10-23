// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A failed balancing try-lock must retain a release-driven retry prompt.

use loom::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use loom::thread;

use crate::balance_work::Work;
use crate::sharded_lock::InterruptShardedLock;
use crate::{balance_retry::ReleaseRetry, require_ok};

loom::lazy_static! {
    static ref RETRY: ReleaseRetry = ReleaseRetry::new();
    static ref PROMPTS: AtomicUsize = AtomicUsize::new(0);
}

struct Mask;
impl hyper::hal::interrupt::InterruptMask for Mask {
    type State = ();
    fn save_and_disable() {}
    fn restore(_: ()) {}
    fn wait_for_lock_owner() {
        thread::yield_now();
    }
}

fn release_prompt() {
    if let Some(cpu) = RETRY.released() {
        assert_eq!(cpu, 1);
        PROMPTS.fetch_add(1, Ordering::Release);
    }
}

fn delayed_release_prompt() {
    thread::yield_now();
    if let Some(cpu) = RETRY.released() {
        PROMPTS.fetch_or(1 << cpu, Ordering::Release);
    }
}

#[test]
fn reader_release_and_optional_writer_cannot_both_miss_the_handoff() {
    release_races_try(false);
}

#[test]
fn writer_release_and_optional_writer_cannot_both_miss_the_handoff() {
    release_races_try(true);
}

fn release_races_try(writer: bool) {
    loom::model(move || {
        let lock = Arc::new(InterruptShardedLock::<_, Mask, 2>::new_notifying(
            (),
            release_prompt,
        ));
        let entered = Arc::new(AtomicBool::new(false));
        let releaser = {
            let lock = lock.clone();
            let entered = entered.clone();
            thread::spawn(move || {
                let hold = || {
                    entered.store(true, Ordering::Release);
                    thread::yield_now();
                };
                if writer {
                    lock.with(|_| hold());
                } else {
                    assert_eq!(lock.read(1, |_| hold()), Some(()));
                }
            })
        };
        while !entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        RETRY.register(1);
        let acquired = lock.try_with(|_| RETRY.acquired()).is_some();
        require_ok(releaser.join());
        assert!(acquired || PROMPTS.load(Ordering::Acquire) > 0);
        // Retry and unrelated releases cannot perpetually regenerate prompts.
        let before = PROMPTS.load(Ordering::Acquire);
        assert_eq!(lock.try_with(|_| RETRY.acquired()), Some(()));
        assert_eq!(lock.read(0, |_| ()), Some(()));
        assert_eq!(PROMPTS.load(Ordering::Acquire), before);
    });
}

#[test]
fn absent_or_consumed_waiters_do_not_prompt_again() {
    loom::model(|| {
        let retry = ReleaseRetry::new();
        assert_eq!(retry.released(), None);
        retry.register(2);
        assert_eq!(retry.released(), Some(2));
        assert_eq!(retry.released(), None);
        retry.register(3);
        retry.acquired();
        assert_eq!(retry.released(), None);
    });
}

#[test]
fn repeated_failures_coalesce_on_the_blocking_lane_release() {
    loom::model(|| {
        let lock = Arc::new(InterruptShardedLock::<_, Mask, 2>::new_notifying(
            (),
            release_prompt,
        ));
        let held = lock.read(1, |_| {
            let lock = lock.clone();
            require_ok(
                thread::spawn(move || {
                    RETRY.register(1);
                    assert_eq!(lock.try_with(|_| ()), None);
                    assert_eq!(lock.try_with(|_| ()), None);
                    assert_eq!(PROMPTS.load(Ordering::Acquire), 0);
                })
                .join(),
            );
        });
        assert_eq!(held, Some(()));
        assert_eq!(PROMPTS.load(Ordering::Acquire), 1);
        assert_eq!(lock.try_with(|_| ()), Some(()));
        assert_eq!(PROMPTS.load(Ordering::Acquire), 1);
    });
}

#[test]
fn new_request_and_active_pass_exit_cannot_both_miss_a_prompt() {
    loom::model(|| {
        let work = Arc::new(Work::new());
        let prompted = Arc::new(AtomicBool::new(false));
        assert!(work.request());
        assert!(work.begin());
        let requester = {
            let work = work.clone();
            thread::spawn(move || {
                if work.request() && work.begin() {
                    assert!(!work.complete(false));
                }
                // A prompt delivered while the previous pass owns ACTIVE is
                // consumed here, so the owner's exit must carry the retry.
            })
        };
        if work.complete(false) {
            prompted.store(true, Ordering::Release);
        }
        require_ok(requester.join());
        assert!(!work.pending() || prompted.load(Ordering::Acquire));
    });
}

#[test]
fn requests_do_not_restart_an_unfinished_sweep_and_quiescence_stays_quiet() {
    loom::model(|| {
        let work = Work::new();
        assert!(!work.pending());
        assert!(!work.begin());
        assert!(work.request());
        assert!(work.begin());
        assert!(!work.request());
        assert!(work.complete(true));
        assert!(work.begin());
        assert!(work.complete(false));
        assert!(work.begin());
        work.blocked();
        assert!(work.pending());
        assert!(work.begin());
        assert!(!work.complete(false));
        assert!(!work.pending());
        assert!(!work.begin());
    });
}

#[test]
fn delayed_old_callback_still_prompts_a_new_serialized_attempt() {
    loom::model(|| {
        let lock = Arc::new(InterruptShardedLock::<_, Mask, 2>::new_notifying(
            (),
            delayed_release_prompt,
        ));
        let (entered, arrivals) = loom::sync::mpsc::channel();
        let mut readers = Vec::new();
        let mut releases = Vec::new();
        for lane in 0..2 {
            let lock = lock.clone();
            let entered = entered.clone();
            let (release, released) = loom::sync::mpsc::channel();
            releases.push(release);
            readers.push(thread::spawn(move || {
                assert_eq!(
                    lock.read(lane, |_| {
                        require_ok(entered.send(()));
                        require_ok(released.recv());
                    }),
                    Some(())
                );
            }));
        }
        require_ok(arrivals.recv());
        require_ok(arrivals.recv());
        RETRY.register(1);
        assert_eq!(lock.try_with(|_| ()), None);
        require_ok(releases[0].send(()));
        RETRY.register(2);
        let acquired = lock.try_with(|_| RETRY.acquired()).is_some();
        require_ok(releases[1].send(()));
        for reader in readers {
            require_ok(reader.join());
        }
        assert!(acquired || PROMPTS.load(Ordering::Acquire) & (1 << 2) != 0);
    });
}
