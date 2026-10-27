// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Atomically tagged pointer state for short-lived exclusive borrows.

use core::marker::PhantomData;
use core::ptr::NonNull;

use super::atomic::{AtomicPtr, Ordering};

const BORROWED_TAG: usize = 1;

/// Failure to perform an exact tagged-pointer state transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicBorrowError {
    Active,
    Borrowed,
    Inactive,
    InvalidPointer,
    NotBorrowed,
    PointerMismatch,
}

/// One atomically coherent inactive/active/borrowed pointer state.
///
/// This primitive serializes only the pointer-state transitions. It neither
/// owns nor synchronizes access to the pointee. A user which dereferences a
/// claimed pointer must separately prove its lifetime, pinning, and exclusive
/// access. The relaxed operations intentionally provide no cross-CPU pointee
/// publication; users add the narrower compiler or memory ordering required by
/// their ownership protocol.
pub struct AtomicBorrowPtr<T> {
    state: AtomicPtr<T>,
}

/// Linear authority to observe and finish one exact borrowed-pointer state.
///
/// The claim is neither `Send` nor `Sync`. Dropping it normally restores the
/// active state. Deliberately forgetting it leaves the pointer Borrowed, which
/// leaks availability but cannot create a second completion authority.
pub struct AtomicBorrowClaim<'slot, T> {
    owner: &'slot AtomicBorrowPtr<T>,
    pointer: NonNull<T>,
    armed: bool,
    not_send: PhantomData<*mut ()>,
}

impl<T> AtomicBorrowPtr<T> {
    pub const fn new() -> Self {
        Self {
            state: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Publishes `pointer` only when the state is inactive.
    pub fn publish(&self, pointer: NonNull<T>) -> Result<(), AtomicBorrowError> {
        let pointer = validate_pointer(pointer)?;
        self.state
            .compare_exchange(
                core::ptr::null_mut(),
                pointer,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .map(|_| ())
            .map_err(|_| AtomicBorrowError::Active)
    }

    /// Changes an active pointer into its borrowed state.
    ///
    /// `Ok(None)` denotes an inactive state. A successful pointer is still raw
    /// ownership evidence; this type never authorizes dereferencing it.
    pub fn begin_borrow(&self) -> Result<Option<AtomicBorrowClaim<'_, T>>, AtomicBorrowError> {
        let mut observed = self.state.load(Ordering::Relaxed);
        loop {
            if observed.is_null() {
                return Ok(None);
            }
            if is_borrowed(observed) {
                return Err(AtomicBorrowError::Borrowed);
            }
            let borrowed = tag_borrowed(observed);
            match self.state.compare_exchange_weak(
                observed,
                borrowed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let pointer = NonNull::new(observed).ok_or(AtomicBorrowError::Inactive)?;
                    return Ok(Some(AtomicBorrowClaim {
                        owner: self,
                        pointer,
                        armed: true,
                        not_send: PhantomData,
                    }));
                }
                Err(current) => observed = current,
            }
        }
    }

    /// Removes the exact active pointer without disturbing another state.
    pub fn unpublish(&self, pointer: NonNull<T>) -> Result<(), AtomicBorrowError> {
        let pointer = validate_pointer(pointer)?;
        match self.state.compare_exchange(
            pointer,
            core::ptr::null_mut(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(observed) if observed.is_null() => Err(AtomicBorrowError::Inactive),
            Err(observed) if is_borrowed(observed) => Err(AtomicBorrowError::Borrowed),
            Err(_) => Err(AtomicBorrowError::PointerMismatch),
        }
    }
}

impl<T> AtomicBorrowClaim<'_, T> {
    /// Observes the claimed pointer without granting completion authority.
    pub const fn pointer(&self) -> NonNull<T> {
        self.pointer
    }

    /// Consumes this claim and restores its exact pointer to Active.
    pub fn finish(mut self) -> Result<(), AtomicBorrowError> {
        let result = self.finish_inner();
        // A failed exact transition must not be retried by Drop: the observed
        // state may now belong to a different protocol generation.
        self.armed = false;
        result
    }

    fn finish_inner(&mut self) -> Result<(), AtomicBorrowError> {
        let pointer = validate_pointer(self.pointer)?;
        let borrowed = tag_borrowed(pointer);
        match self.owner.state.compare_exchange(
            borrowed,
            pointer,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(observed) => Err(classify_completion_failure(observed, pointer)),
        }
    }
}

impl<T> Drop for AtomicBorrowClaim<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.finish_inner();
            self.armed = false;
        }
    }
}

impl<T> Default for AtomicBorrowPtr<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_pointer<T>(pointer: NonNull<T>) -> Result<*mut T, AtomicBorrowError> {
    let pointer = pointer.as_ptr();
    if pointer.addr() & BORROWED_TAG == 0 {
        Ok(pointer)
    } else {
        Err(AtomicBorrowError::InvalidPointer)
    }
}

fn is_borrowed<T>(pointer: *mut T) -> bool {
    pointer.addr() & BORROWED_TAG != 0
}

fn tag_borrowed<T>(pointer: *mut T) -> *mut T {
    pointer.map_addr(|address| address | BORROWED_TAG)
}

fn classify_completion_failure<T>(observed: *mut T, expected: *mut T) -> AtomicBorrowError {
    if observed.is_null() {
        return AtomicBorrowError::Inactive;
    }
    if !is_borrowed(observed) {
        return if observed == expected {
            AtomicBorrowError::NotBorrowed
        } else {
            AtomicBorrowError::PointerMismatch
        };
    }
    let active = observed.map_addr(|address| address & !BORROWED_TAG);
    if active == expected {
        AtomicBorrowError::NotBorrowed
    } else {
        AtomicBorrowError::PointerMismatch
    }
}
