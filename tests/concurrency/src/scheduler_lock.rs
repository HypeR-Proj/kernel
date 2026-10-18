// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional scheduler maintenance must release partially claimed reader lanes.

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

use crate::require_ok;
use crate::sharded_lock::InterruptShardedLock;

struct Mask;

impl hyper::hal::interrupt::InterruptMask for Mask {
    type State = ();
    fn save_and_disable() {}
    fn restore(_: ()) {}
    fn wait_for_lock_owner() {
        thread::yield_now();
    }
}

#[test]
fn optional_writer_never_overlaps_a_reader_or_loses_partial_lanes() {
    loom::model(|| {
        let lock = Arc::new(InterruptShardedLock::<_, Mask, 2>::new(UnsafeCell::new(0)));
        let reader = {
            let lock = lock.clone();
            thread::spawn(move || {
                assert_eq!(
                    lock.read(1, |value| {
                        value.with(|pointer| {
                            // SAFETY: this production read lane excludes writers.
                            let before = unsafe { *pointer };
                            thread::yield_now();
                            // SAFETY: the same lane remains held.
                            assert_eq!(unsafe { *pointer }, before);
                        });
                    }),
                    Some(())
                );
            })
        };
        let written = lock.try_with(|value| {
            value.with_mut(|pointer| {
                // SAFETY: the production writer owns every reader lane.
                unsafe { *pointer = 17 };
            });
        });
        // Observe through a different reader lane before joining the peer.
        assert_eq!(
            lock.read(0, |value| value.with(|pointer| {
                // SAFETY: reader lane zero excludes all writers.
                unsafe { *pointer }
            })),
            Some(if written.is_some() { 17 } else { 0 })
        );
        require_ok(reader.join());
        assert_eq!(lock.try_with(|_| ()), Some(()));
    });
}

#[test]
fn competing_optional_and_required_writers_preserve_publication_and_reuse() {
    loom::model(|| {
        let lock = Arc::new(InterruptShardedLock::<_, Mask, 2>::new(UnsafeCell::new(0)));
        let required = {
            let lock = lock.clone();
            thread::spawn(move || {
                lock.with(|value| {
                    value.with_mut(|pointer| {
                        // SAFETY: all lanes exclude competing writers/readers.
                        unsafe { *pointer += 1 };
                    })
                });
            })
        };
        let optional = lock.try_with(|value| {
            value.with_mut(|pointer| {
                // SAFETY: all lanes exclude competing writers/readers.
                unsafe { *pointer += 2 };
            })
        });
        require_ok(required.join());
        assert_eq!(
            lock.try_with(|value| value.with(|pointer| {
                // SAFETY: exclusive lock ownership protects the payload.
                unsafe { *pointer }
            })),
            Some(if optional.is_some() { 3 } else { 1 })
        );
    });
}
