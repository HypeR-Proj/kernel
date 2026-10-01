// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic interleavings of the production reference-count transitions.

use super::{
    PROBING, StrongRelease, release_strong, release_weak, retain_strong, retain_weak, strong_count,
    try_unique, try_unique_inner, try_upgrade,
};
use core::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn unobserved_sole_owner_becomes_unique() {
    let strong = AtomicUsize::new(1);
    let weak = AtomicUsize::new(1);
    assert!(try_unique(&strong, &weak));
    assert_eq!(strong.load(Ordering::Relaxed), 0);
    assert_eq!(weak.load(Ordering::Relaxed), 1);
}

#[test]
fn shared_or_weak_observed_owner_cannot_become_unique() {
    let strong = AtomicUsize::new(2);
    let weak = AtomicUsize::new(1);
    assert!(!try_unique(&strong, &weak));
    assert_eq!(strong_count(&strong), 2);

    retain_weak(&weak);
    assert_eq!(release_strong(&strong), StrongRelease::Shared);
    assert!(!try_unique(&strong, &weak));
    assert!(try_upgrade(&strong));
    assert_eq!(strong_count(&strong), 2);
}

#[test]
fn downgrade_and_release_after_weak_precheck_rejects_unique_conversion() {
    let strong = AtomicUsize::new(2);
    let weak = AtomicUsize::new(1);
    // This is the old race: A reads weak == 1; B creates a weak observer and
    // drops the other strong owner; A then finds strong == 1. The post-claim
    // weak check must return A's original live owner instead of a unique token.
    assert!(!try_unique_inner(
        &strong,
        &weak,
        || {
            retain_weak(&weak);
            assert_eq!(release_strong(&strong), StrongRelease::Shared);
        },
        || assert_eq!(strong.load(Ordering::Relaxed), PROBING),
    ));
    assert_eq!(strong_count(&strong), 1);
    assert!(try_upgrade(&strong));
    assert_eq!(release_strong(&strong), StrongRelease::Shared);
    assert_eq!(release_strong(&strong), StrongRelease::Final);
    assert!(!try_upgrade(&strong));
    assert!(!release_weak(&weak));
    assert!(release_weak(&weak));
}

#[test]
fn upgrade_cancels_unique_probe_without_observing_a_dead_value() {
    let strong = AtomicUsize::new(2);
    let weak = AtomicUsize::new(1);
    assert!(!try_unique_inner(
        &strong,
        &weak,
        || {
            retain_weak(&weak);
            assert_eq!(release_strong(&strong), StrongRelease::Shared);
        },
        || {
            assert_eq!(strong_count(&strong), 1);
            assert!(try_upgrade(&strong));
            assert_eq!(strong_count(&strong), 2);
            // Even after the observer disappears, the acquired strong owner
            // must make the final probe CAS fail.
            assert!(!release_weak(&weak));
        },
    ));
    assert_eq!(strong_count(&strong), 2);
    assert_eq!(weak.load(Ordering::Relaxed), 1);
    assert_eq!(release_strong(&strong), StrongRelease::Shared);
    assert_eq!(release_strong(&strong), StrongRelease::Final);
    assert!(release_weak(&weak));
}

#[test]
fn upgrade_and_release_back_to_one_still_cancels_the_probe() {
    let strong = AtomicUsize::new(2);
    let weak = AtomicUsize::new(1);
    assert!(!try_unique_inner(
        &strong,
        &weak,
        || {
            retain_weak(&weak);
            assert_eq!(release_strong(&strong), StrongRelease::Shared);
        },
        || {
            assert!(try_upgrade(&strong));
            assert!(!release_weak(&weak));
            assert_eq!(release_strong(&strong), StrongRelease::Shared);
            assert_eq!(strong_count(&strong), 1);
        },
    ));
    // A failed probe must not confuse one surviving owner with its marker,
    // even when the weak observer and upgraded owner have both been dropped.
    assert_eq!(strong_count(&strong), 1);
    assert_eq!(weak.load(Ordering::Relaxed), 1);
    assert!(try_unique(&strong, &weak));
}

#[test]
fn dropping_observer_during_probe_allows_unique_conversion() {
    let strong = AtomicUsize::new(2);
    let weak = AtomicUsize::new(1);
    assert!(try_unique_inner(
        &strong,
        &weak,
        || {
            retain_weak(&weak);
            assert_eq!(release_strong(&strong), StrongRelease::Shared);
        },
        || assert!(!release_weak(&weak)),
    ));
    assert_eq!(strong.load(Ordering::Relaxed), 0);
    assert_eq!(weak.load(Ordering::Relaxed), 1);
}

#[test]
fn clone_and_upgrade_saturate_without_entering_probe_state() {
    for upgrade in [false, true] {
        let strong = AtomicUsize::new(PROBING - 1);
        if upgrade {
            assert!(try_upgrade(&strong));
        } else {
            retain_strong(&strong);
        }
        assert_eq!(strong_count(&strong), usize::MAX);
        retain_strong(&strong);
        assert!(try_upgrade(&strong));
        assert_eq!(release_strong(&strong), StrongRelease::Leaked);
        assert!(!try_unique(&strong, &AtomicUsize::new(1)));
        assert_eq!(strong_count(&strong), usize::MAX);
    }
}

#[test]
fn weak_counter_keeps_its_full_saturating_range() {
    let weak = AtomicUsize::new(PROBING - 1);
    retain_weak(&weak);
    assert_eq!(weak.load(Ordering::Relaxed), PROBING);
    assert!(!release_weak(&weak));
    assert_eq!(weak.load(Ordering::Relaxed), PROBING - 1);
    retain_weak(&weak);
    retain_weak(&weak);
    assert_eq!(weak.load(Ordering::Relaxed), usize::MAX);
    retain_weak(&weak);
    assert!(!release_weak(&weak));
    assert_eq!(weak.load(Ordering::Relaxed), usize::MAX);
}

#[test]
fn dead_counts_do_not_upgrade_or_underflow() {
    let strong = AtomicUsize::new(0);
    let weak = AtomicUsize::new(0);
    assert!(!try_upgrade(&strong));
    assert_eq!(release_strong(&strong), StrongRelease::Leaked);
    assert!(!release_weak(&weak));
    assert_eq!(strong_count(&strong), 0);
    assert_eq!(weak.load(Ordering::Relaxed), 0);
}
