// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-capacity storage used by CPU-local slab magazines.
//!
//! This module contains no allocator, pointer, CPU, or synchronization policy.
//! It only moves uniquely owned values between bounded containers so the
//! global adapter can keep ownership transfers explicit and allocation-free.

pub(super) const MAGAZINE_STORAGE: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum PushError<T> {
    Full(T),
    InvalidState(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidState;

pub(super) struct Magazine<T> {
    entries: [Option<T>; MAGAZINE_STORAGE],
    length: usize,
}

impl<T> Magazine<T> {
    pub(super) const fn new() -> Self {
        Self {
            entries: [const { None }; MAGAZINE_STORAGE],
            length: 0,
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.length
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub(super) fn push(&mut self, value: T, limit: usize) -> Result<(), PushError<T>> {
        if limit > MAGAZINE_STORAGE || self.length > MAGAZINE_STORAGE {
            return Err(PushError::InvalidState(value));
        }
        if self.length >= limit {
            return Err(PushError::Full(value));
        }
        if self.entries[self.length].is_some() {
            return Err(PushError::InvalidState(value));
        }
        self.entries[self.length] = Some(value);
        self.length += 1;
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Result<Option<T>, InvalidState> {
        if self.length > MAGAZINE_STORAGE {
            return Err(InvalidState);
        }
        let Some(index) = self.length.checked_sub(1) else {
            return Ok(None);
        };
        let Some(value) = self.entries[index].take() else {
            return Err(InvalidState);
        };
        self.length = index;
        Ok(Some(value))
    }

    /// Moves at most `count` values into an independent transfer batch.
    pub(super) fn take(&mut self, count: usize) -> Result<Self, InvalidState> {
        let mut batch = Self::new();
        let transfer = count.min(self.length);
        for _ in 0..transfer {
            let Some(value) = self.pop()? else {
                break;
            };
            match batch.push(value, MAGAZINE_STORAGE) {
                Ok(()) => {}
                Err(PushError::Full(_)) | Err(PushError::InvalidState(_)) => {
                    return Err(InvalidState);
                }
            }
        }
        Ok(batch)
    }
}
