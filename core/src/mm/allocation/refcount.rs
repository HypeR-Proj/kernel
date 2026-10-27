// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Atomic ownership transitions; allocation and destruction stay with the owner.

use core::sync::atomic::{AtomicUsize, Ordering, fence};

// Only the strong counter reserves this value. usize::MAX still means a
// permanently leaked allocation, for both strong and weak counters.
const PROBING: usize = usize::MAX - 1;

pub(super) fn strong_count(strong: &AtomicUsize) -> usize {
    match strong.load(Ordering::Relaxed) {
        PROBING => 1,
        count => count,
    }
}

pub(super) fn retain_strong(strong: &AtomicUsize) {
    let mut current = strong.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            return;
        }
        if current == PROBING {
            unexpected_strong_owner();
        }
        match strong.compare_exchange_weak(
            current,
            increment_strong(current),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn try_upgrade(strong: &AtomicUsize) -> bool {
    let mut current = strong.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return false;
        }
        if current == usize::MAX {
            return true;
        }
        // The probing owner is still alive. Cancel its conversion and acquire
        // a second real reference, without waiting for that CPU to resume.
        let next = if current == PROBING {
            2
        } else {
            increment_strong(current)
        };
        match strong.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn try_unique(strong: &AtomicUsize, weak: &AtomicUsize) -> bool {
    try_unique_inner(
        strong,
        weak,
        #[cfg(test)]
        || {},
        #[cfg(test)]
        || {},
    )
}

fn try_unique_inner(
    strong: &AtomicUsize,
    weak: &AtomicUsize,
    #[cfg(test)] before_claim: impl FnOnce(),
    #[cfg(test)] after_claim: impl FnOnce(),
) -> bool {
    // Reject existing observers cheaply. This check alone is insufficient:
    // another strong owner can downgrade and drop before the following CAS.
    if weak.load(Ordering::Acquire) != 1 {
        return false;
    }
    #[cfg(test)]
    before_claim();

    // Acquire observes previous owners' releasing decrements, including weak
    // references they created before dropping their strong ownership. Once
    // claimed, no other strong owner can create a new weak reference unless a
    // weak upgrade first cancels this probe.
    if strong
        .compare_exchange(1, PROBING, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    #[cfg(test)]
    after_claim();

    // Both checks are needed: a weak owner may upgrade and drop back to one
    // while the probe runs. That cancellation must never grant unique access.
    if weak.load(Ordering::Acquire) == 1
        && strong
            .compare_exchange(PROBING, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    {
        return true;
    }
    // Do not overwrite an upgrade's ownership. Failed conversion never
    // publishes zero, so observers cannot see temporary death/resurrection.
    let _ = strong.compare_exchange(PROBING, 1, Ordering::Release, Ordering::Relaxed);
    false
}

fn increment_strong(current: usize) -> usize {
    if current == PROBING - 1 {
        usize::MAX
    } else {
        current + 1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StrongRelease {
    Shared,
    Final,
    Leaked,
}

pub(super) fn release_strong(strong: &AtomicUsize) -> StrongRelease {
    let mut current = strong.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX || current == 0 {
            // Preserve leak-on-saturation and defensive zero-count handling.
            return StrongRelease::Leaked;
        }
        if current == PROBING {
            unexpected_strong_owner();
        }
        match strong.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(1) => {
                // The final owner observes prior owners' writes before the
                // caller accesses or destroys the initialized value.
                fence(Ordering::Acquire);
                return StrongRelease::Final;
            }
            Ok(_) => return StrongRelease::Shared,
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn retain_weak(weak: &AtomicUsize) {
    let mut current = weak.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            return;
        }
        match weak.compare_exchange_weak(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Returns whether this caller released the final allocation reference.
pub(super) fn release_weak(weak: &AtomicUsize) -> bool {
    let mut current = weak.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX || current == 0 {
            return false;
        }
        match weak.compare_exchange_weak(current, current - 1, Ordering::Release, Ordering::Relaxed)
        {
            Ok(1) => {
                fence(Ordering::Acquire);
                return true;
            }
            Ok(_) => return false,
            Err(observed) => current = observed,
        }
    }
}

#[cold]
fn unexpected_strong_owner() -> ! {
    // The probe consumed the sole shared owner. Any new owner must cancel it
    // through try_upgrade before a normal clone or release can be legal.
    crate::debug::invariant_failure("FallibleArc strong owner during unique probe")
}

#[cfg(test)]
#[path = "../../../../tests/host/src/cases/fallible_refcount.rs"]
mod tests;
