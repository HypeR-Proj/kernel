// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Ownership and destruction behavior of fallible allocation helpers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct ZeroSizedDrop;

struct CountedDrop(Arc<AtomicUsize>);

impl Drop for CountedDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for ZeroSizedDrop {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn try_box_moves_and_drops_zero_sized_values_once() {
    DROPS.store(0, Ordering::Relaxed);
    let value = crate::require_ok(hyper::mm::try_box(ZeroSizedDrop));
    assert_eq!(DROPS.load(Ordering::Relaxed), 0);
    drop(value);
    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
}

#[test]
fn fallible_arc_clones_share_one_stable_value() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(41_u64));
    let clone = owner.clone();

    assert_eq!(*owner, 41);
    assert_eq!(*clone, 41);
    assert_eq!(owner.strong_count(), 2);
}

#[test]
fn weak_fallible_arc_observes_value_lifetime_without_owning_it() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(String::from("live")));
    let observer = owner.downgrade();
    let upgraded = crate::require_some(observer.upgrade());
    assert_eq!(&*upgraded, "live");
    drop(upgraded);
    drop(owner);
    assert!(observer.upgrade().is_none());
}

#[test]
fn external_weak_owner_prevents_unique_arc_conversion() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(41_u64));
    let observer = owner.downgrade();
    let owner = match owner.try_into_unique() {
        Ok(_) => panic!("weak-observed owner unexpectedly became unique"),
        Err(owner) => owner,
    };
    drop(observer);
    let unique = match owner.try_into_unique() {
        Ok(unique) => unique,
        Err(_) => panic!("unobserved owner did not become unique"),
    };
    assert_eq!(*unique, 41);
}

#[test]
fn weak_allocation_outlives_but_does_not_retain_the_value() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let observer = owner.downgrade();
    drop(owner);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(observer.upgrade().is_none());
    drop(observer);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn concurrent_weak_upgrade_never_observes_a_destroyed_value() {
    use std::thread;

    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let weak = owner.downgrade();
    let start = Arc::new(Barrier::new(2));
    let worker_start = start.clone();
    let worker_drops = drops.clone();
    let worker = thread::spawn(move || {
        worker_start.wait();
        while let Some(observed) = weak.upgrade() {
            assert_eq!(worker_drops.load(Ordering::Acquire), 0);
            drop(observed);
            thread::yield_now();
        }
    });

    start.wait();
    drop(owner);
    match worker.join() {
        Ok(()) => {}
        Err(_) => panic!("weak-upgrade worker panicked"),
    }
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn deferred_release_closes_weak_upgrade_before_destroying_the_value() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let observer = owner.downgrade();

    let deferred = match owner.release_deferred() {
        Some(deferred) => deferred,
        None => panic!("sole shared owner did not win final release"),
    };
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert!(!observer.is_alive());
    assert!(observer.upgrade().is_none());

    drop(deferred);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(observer.upgrade().is_none());
}

#[test]
fn nonfinal_deferred_release_leaves_the_remaining_owner_live() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let peer = owner.clone();

    assert!(owner.release_deferred().is_none());
    assert_eq!(peer.strong_count(), 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    let deferred = match peer.release_deferred() {
        Some(deferred) => deferred,
        None => panic!("remaining owner did not win final release"),
    };
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(deferred);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn concurrent_deferred_releases_produce_exactly_one_final_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let observer = owner.downgrade();
    let barrier = Arc::new(Barrier::new(5));
    let mut owners = Vec::new();
    owners.push(owner);
    for _ in 0..3 {
        let clone = owners[0].clone();
        owners.push(clone);
    }

    let mut workers = Vec::new();
    for owner in owners {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            owner.release_deferred()
        }));
    }
    barrier.wait();

    let mut deferred = None;
    let mut winners = 0usize;
    for worker in workers {
        let result = match worker.join() {
            Ok(result) => result,
            Err(_) => panic!("deferred-release worker panicked"),
        };
        if let Some(owner) = result {
            winners += 1;
            deferred = Some(owner);
        }
    }

    assert_eq!(winners, 1);
    assert!(!observer.is_alive());
    assert!(observer.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(deferred);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn weak_upgrade_racing_final_release_preserves_one_deferred_owner() {
    for _ in 0..64 {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
        let observer = owner.downgrade();
        let contender = observer.clone();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            match contender.upgrade() {
                Some(owner) => owner.release_deferred(),
                None => None,
            }
        });

        barrier.wait();
        let caller_deferred = owner.release_deferred();
        let worker_deferred = match worker.join() {
            Ok(deferred) => deferred,
            Err(_) => panic!("weak-upgrade contender panicked"),
        };

        assert_eq!(
            usize::from(caller_deferred.is_some()) + usize::from(worker_deferred.is_some()),
            1
        );
        assert!(!observer.is_alive());
        assert!(observer.upgrade().is_none());
        assert_eq!(drops.load(Ordering::Acquire), 0);

        drop(caller_deferred);
        drop(worker_deferred);
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }
}

#[test]
fn unique_fallible_arc_extracts_without_reallocation() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(String::from("retired")));
    let value = match owner.try_unwrap() {
        Ok(value) => value,
        Err(_) => panic!("unique owner was not extracted"),
    };
    assert_eq!(value, "retired");
}

#[test]
fn failed_extraction_returns_the_original_live_owner() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(41_u64));
    let peer = owner.clone();
    let owner = match owner.try_unwrap() {
        Ok(_) => panic!("shared owner unexpectedly extracted"),
        Err(owner) => owner,
    };
    assert_eq!(*owner, 41);
    assert_eq!(owner.strong_count(), 2);
    drop(peer);
    assert_eq!(owner.strong_count(), 1);
    let value = match owner.try_unwrap() {
        Ok(value) => value,
        Err(_) => panic!("unique owner was not extracted after peer release"),
    };
    assert_eq!(value, 41);
}

#[test]
fn unique_arc_retains_address_and_can_restore_sharing() {
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(17_u64));
    let address = core::ptr::from_ref(&*owner);
    let mut unique = match owner.try_into_unique() {
        Ok(unique) => unique,
        Err(_) => panic!("sole shared owner did not become unique"),
    };
    assert_eq!(core::ptr::from_ref(&*unique), address);
    *unique = 23;
    let shared = unique.into_shared();
    assert_eq!(core::ptr::from_ref(&*shared), address);
    assert_eq!(*shared, 23);
}

#[test]
fn uninitialized_unique_arc_initializes_and_drops_once() {
    struct CountedDrop<'a>(&'a AtomicUsize);

    impl Drop for CountedDrop<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = AtomicUsize::new(0);
    let slot = crate::require_ok(hyper::mm::UniqueFallibleArc::try_new_uninit());
    let owner = slot.write(CountedDrop(&drops));
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(owner);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn fallible_arc_destroys_the_value_after_the_last_concurrent_owner() {
    struct CountedDrop(std::sync::Arc<AtomicUsize>);

    impl Drop for CountedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = std::sync::Arc::new(AtomicUsize::new(0));
    let owner = crate::require_ok(hyper::mm::FallibleArc::try_new(CountedDrop(drops.clone())));
    let barrier = std::sync::Arc::new(Barrier::new(5));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let clone = owner.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            drop(clone);
        }));
    }

    barrier.wait();
    for worker in workers {
        assert!(worker.join().is_ok());
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(owner);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
