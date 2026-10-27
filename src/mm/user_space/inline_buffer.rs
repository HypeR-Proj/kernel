// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A single inline metadata entry, with explicitly prepared overflow storage.
//! Insertion never allocates, including while publishing write reservations.

use alloc::collections::TryReserveError;
use alloc::vec::Vec;

pub(super) enum InlineBuffer<T> {
    One(Option<T>),
    Many(Vec<T>),
}

impl<T> Default for InlineBuffer<T> {
    fn default() -> Self {
        Self::One(None)
    }
}

impl<T> InlineBuffer<T> {
    pub(super) fn try_with_capacity(capacity: usize) -> Result<Self, TryReserveError> {
        if capacity <= 1 {
            return Ok(Self::default());
        }
        let mut values = Vec::new();
        values.try_reserve_exact(capacity)?;
        Ok(Self::Many(values))
    }

    pub(super) fn allocated_capacity(capacity: usize) -> usize {
        if capacity <= 1 { 0 } else { capacity }
    }

    pub(super) fn push(&mut self, value: T) {
        match self {
            Self::One(slot @ None) => *slot = Some(value),
            Self::Many(values) if values.len() < values.capacity() => values.push(value),
            _ => hyper::debug::invariant_failure("unprepared user memory metadata capacity"),
        }
    }

    pub(super) fn swap_remove(&mut self, index: usize) {
        match self {
            Self::One(value) if index == 0 && value.is_some() => {
                value.take();
            }
            Self::Many(values) if index < values.len() => {
                values.swap_remove(index);
            }
            _ => hyper::debug::invariant_failure("invalid user memory metadata index"),
        }
    }
}

impl<T> core::ops::Deref for InlineBuffer<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Self::One(value) => value.as_slice(),
            Self::Many(values) => values,
        }
    }
}

impl<T> core::ops::DerefMut for InlineBuffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Self::One(value) => value.as_mut_slice(),
            Self::Many(values) => values,
        }
    }
}
