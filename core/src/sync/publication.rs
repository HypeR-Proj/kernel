// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One-shot immutable publication with explicit release/acquire visibility.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

use super::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const INSTALLING: u8 = 1;
const READY: u8 = 2;

#[derive(Debug, Eq, PartialEq)]
pub struct PublishError<T>(T);

impl<T> PublishError<T> {
    pub fn into_value(self) -> T {
        self.0
    }
}

/// Storage for one value which becomes immutable after its first publication.
///
/// A successful publisher owns initialization after the `EMPTY -> INSTALLING`
/// transition. Its Release store of `READY` publishes every preceding write;
/// readers return a reference only after an Acquire load observes that state.
/// Failed publishers never wait for the winning publisher and cannot inspect
/// a partially initialized value.
pub struct PublishedOnce<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> PublishedOnce<T> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Publishes `value`, or returns it to the caller if another publication
    /// already owns the cell.
    pub fn publish(&self, value: T) -> Result<(), PublishError<T>> {
        if self
            .state
            .compare_exchange(EMPTY, INSTALLING, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Err(PublishError(value));
        }
        // SAFETY: EMPTY -> INSTALLING grants this call the only write. Readers
        // cannot access the storage until the following Release publication.
        unsafe { (*self.value.get()).write(value) };
        self.state.store(READY, Ordering::Release);
        Ok(())
    }

    /// Returns the immutable published value after acquiring its initialization.
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }
        // SAFETY: The Acquire load observed the publisher's READY Release.
        // Publication is one-shot, so the initialized value never changes.
        Some(unsafe { (&*self.value.get()).assume_init_ref() })
    }
}

impl<T> Default for PublishedOnce<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for PublishedOnce<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            // SAFETY: Exclusive access excludes a concurrent publisher or
            // reader, and READY proves the value was initialized exactly once.
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

// SAFETY: T: Send permits the publisher to transfer ownership into shared
// storage. T: Sync permits references acquired after publication to cross
// execution contexts. Release/Acquire orders initialization before all reads.
unsafe impl<T: Send + Sync> Sync for PublishedOnce<T> {}
