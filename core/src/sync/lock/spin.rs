// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Minimal non-reentrant spin lock.

use core::cell::UnsafeCell;
use core::hint::spin_loop;

use crate::sync::atomic::{AtomicBool, Ordering};

/// A small non-reentrant lock for state protected by an external IRQ policy.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

struct LockRelease<'lock>(&'lock AtomicBool);

struct LocksRelease<'a, T> {
    locks: &'a [&'a SpinLock<T>],
    held: usize,
}

impl<T> Drop for LocksRelease<'_, T> {
    fn drop(&mut self) {
        for lock in &self.locks[..self.held] {
            lock.locked.store(false, Ordering::Release);
        }
    }
}

impl Drop for LockRelease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// SAFETY: Access to the contained value is serialized by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Extracts the protected value from an exclusively owned lock.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    pub fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        self.with_relax(operation, spin_loop)
    }

    /// Runs `operation` after acquiring the lock, invoking `relax` after every
    /// failed acquisition so an enclosing synchronization policy can make
    /// architecture-required progress.
    pub(super) fn with_relax<R>(
        &self,
        operation: impl FnOnce(&mut T) -> R,
        mut relax: impl FnMut(),
    ) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            relax();
        }

        let release = LockRelease(&self.locked);
        // SAFETY: This lock is held exclusively until `release` is dropped.
        let result = operation(unsafe { &mut *self.value.get() });
        drop(release);
        result
    }

    /// Acquires every lock before visiting any value, without waiting or allocating.
    /// Contention (including a duplicate lock) releases the acquired prefix and
    /// returns false without invoking `operation`. Callbacks must not yield or
    /// acquire these locks; unwind releases every lock but does not undo mutations.
    pub fn try_with_each(locks: &[&Self], mut operation: impl FnMut(&mut T)) -> bool {
        let mut release = LocksRelease { locks, held: 0 };
        for lock in locks {
            if lock
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return false;
            }
            release.held += 1;
        }
        for lock in locks {
            // SAFETY: Every distinct lock was acquired above and remains held
            // until `release` drops. Each exclusive reference stays in its callback.
            operation(unsafe { &mut *lock.value.get() });
        }
        true
    }

    /// Runs `operation` only when the lock can be acquired immediately.
    ///
    /// Fatal and diagnostic paths use this to avoid waiting on a lock held by
    /// the context that they interrupted.
    pub fn try_with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }

        let release = LockRelease(&self.locked);
        // SAFETY: The successful transition above gives this caller exclusive
        // access until `release` is dropped.
        let result = operation(unsafe { &mut *self.value.get() });
        drop(release);
        Some(result)
    }
}
