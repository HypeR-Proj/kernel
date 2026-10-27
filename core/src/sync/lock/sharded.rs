// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read sharing without a cache line written by every reader.

use core::cell::UnsafeCell;
use core::marker::PhantomData;

use crate::hal::interrupt::InterruptMask;
#[cfg(not(loom))]
use crate::sync::atomic::{AtomicU8, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU8, Ordering};

use super::InterruptMaskGuard;

// Separate readers even on machines with 128-byte cache lines.
#[repr(align(128))]
struct Stripe(AtomicU8);

const LOCKED: u8 = 1;
const NOTIFY: u8 = 2;

struct Release<'a> {
    stripes: &'a [Stripe],
    held: usize,
    notify: Option<fn()>,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut notify = false;
        for stripe in self.stripes[..self.held].iter().rev() {
            if self.notify.is_some() {
                // The waiter and unlock share one atomic modification order.
                // Acquire observes the waiter's publication before its callback.
                notify |= stripe.0.swap(0, Ordering::AcqRel) & NOTIFY != 0;
            } else {
                stripe.0.store(0, Ordering::Release);
            }
        }
        if notify && let Some(callback) = self.notify {
            // Release every owned lane before calling external code. The local
            // interrupt mask is still held; callbacks must only publish work.
            callback();
        }
    }
}

/// IRQ-safe, non-reentrant shared access partitioned into caller-selected lanes.
///
/// Readers acquire just one lane. Writers acquire every lane in ascending
/// order before accessing the value. Use stable CPU indices for local readers;
/// lanes must not be nested, upgraded, or retained across context switches.
/// Writers are intended for infrequent ownership changes, not hot-path data.
pub struct InterruptShardedLock<T, M: InterruptMask, const N: usize> {
    stripes: [Stripe; N],
    value: UnsafeCell<T>,
    policy: PhantomData<fn() -> M>,
    notify: Option<fn()>,
}

// SAFETY: a writer excludes every reader and writer. Concurrent readers see
// only shared references; T: Sync permits their simultaneous use.
unsafe impl<T: Send + Sync, M: InterruptMask, const N: usize> Sync
    for InterruptShardedLock<T, M, N>
{
}

impl<T, M: InterruptMask, const N: usize> InterruptShardedLock<T, M, N> {
    #[cfg(not(loom))]
    pub const fn new(value: T) -> Self {
        assert!(N > 0);
        Self {
            stripes: [const { Stripe(AtomicU8::new(0)) }; N],
            value: UnsafeCell::new(value),
            policy: PhantomData,
            notify: None,
        }
    }

    #[cfg(loom)]
    pub fn new(value: T) -> Self {
        assert!(N > 0);
        Self {
            stripes: core::array::from_fn(|_| Stripe(AtomicU8::new(0))),
            value: UnsafeCell::new(value),
            policy: PhantomData,
            notify: None,
        }
    }

    /// Gives failed optional writers a release-driven notification.
    ///
    /// A failed `try_with` atomically subscribes to the lane that blocked it.
    /// Its next release calls `notify` once, coalescing all such subscriptions.
    /// The callback runs IRQ-masked and must neither block nor acquire this
    /// lock. It is a progress prompt, not ownership or permission to access T.
    #[cfg(not(loom))]
    pub const fn new_notifying(value: T, notify: fn()) -> Self {
        let mut lock = Self::new(value);
        lock.notify = Some(notify);
        lock
    }

    #[cfg(loom)]
    pub fn new_notifying(value: T, notify: fn()) -> Self {
        let mut lock = Self::new(value);
        lock.notify = Some(notify);
        lock
    }

    /// Reads under one lane; an invalid lane returns `None` without access.
    pub fn read<R>(&self, lane: usize, operation: impl FnOnce(&T) -> R) -> Option<R> {
        let stripe = self.stripes.get(lane)?;
        // SAFETY: the guard remains lexical and no mask escapes this call.
        let mask = unsafe { InterruptMaskGuard::<M>::acquire() };
        Self::acquire(stripe);
        let release = Release {
            stripes: core::slice::from_ref(stripe),
            held: 1,
            notify: self.notify,
        };
        // SAFETY: every writer must acquire this stripe before mutation.
        let result = operation(unsafe { &*self.value.get() });
        drop(release);
        drop(mask);
        Some(result)
    }

    /// Mutates only after excluding all reader lanes.
    pub fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: all stripe ownership ends before this lexical mask restores.
        let mask = unsafe { InterruptMaskGuard::<M>::acquire() };
        let mut release = Release {
            stripes: &self.stripes,
            held: 0,
            notify: self.notify,
        };
        for stripe in &self.stripes {
            Self::acquire(stripe);
            release.held += 1;
        }
        // SAFETY: owning every stripe excludes every other access to value.
        let result = operation(unsafe { &mut *self.value.get() });
        drop(release);
        drop(mask);
        result
    }

    /// Attempts exclusive access without waiting for any reader or writer.
    ///
    /// Contention releases every acquired lane before restoring interrupts.
    /// This is suitable for optional maintenance that may defer to a later
    /// pass; callers must not upgrade a retained reader lane.
    pub fn try_with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Option<R> {
        // SAFETY: partial and complete stripe ownership end before this
        // lexical guard restores the caller's interrupt state.
        let mask = unsafe { InterruptMaskGuard::<M>::acquire() };
        let mut release = Release {
            stripes: &self.stripes,
            held: 0,
            notify: self.notify,
        };
        for stripe in &self.stripes {
            if !Self::try_acquire(stripe, self.notify.is_some()) {
                return None;
            }
            release.held += 1;
        }
        // SAFETY: owning every stripe excludes all other access to value.
        let result = operation(unsafe { &mut *self.value.get() });
        drop(release);
        drop(mask);
        Some(result)
    }

    fn acquire(stripe: &Stripe) {
        while stripe
            .0
            .compare_exchange_weak(0, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while stripe.0.load(Ordering::Relaxed) & LOCKED != 0 {
                M::wait_for_lock_owner();
            }
        }
    }

    fn try_acquire(stripe: &Stripe, notify: bool) -> bool {
        let mut state = 0;
        loop {
            let next = if state == 0 { LOCKED } else { state | NOTIFY };
            let success = if state == 0 {
                Ordering::Acquire
            } else {
                Ordering::Release
            };
            match stripe
                .0
                .compare_exchange(state, next, success, Ordering::Relaxed)
            {
                Ok(_) => return state == 0,
                Err(_) if !notify => return false,
                Err(observed) => state = observed,
            }
        }
    }
}
