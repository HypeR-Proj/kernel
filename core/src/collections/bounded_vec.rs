// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Preallocated vectors with an immutable logical capacity limit.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    Capacity,
}

/// Heap-backed storage whose allocation is completed before runtime use.
pub struct BoundedVec<T> {
    entries: Vec<T>,
    limit: usize,
}

impl<T> BoundedVec<T> {
    pub fn try_new(limit: usize) -> Result<Self, Error> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(limit)
            .map_err(|_| Error::Allocation)?;
        Ok(Self { entries, limit })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn allocation_size(&self) -> Option<usize> {
        self.entries
            .capacity()
            .checked_mul(core::mem::size_of::<T>())
    }

    pub fn remaining(&self) -> usize {
        self.limit - self.entries.len()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.entries.get(index)
    }

    pub fn first(&self) -> Option<&T> {
        self.entries.first()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.entries.iter()
    }

    pub fn push(&mut self, value: T) -> Result<(), Error> {
        if self.entries.len() == self.limit {
            return Err(Error::Capacity);
        }
        // `try_new` reserved the immutable limit before publication, so this
        // push never enters Vec's allocation path.
        self.entries.push(value);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.entries.pop()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn swap(&mut self, left: usize, right: usize) {
        self.entries.swap(left, right);
    }
}

impl<T> core::ops::Index<usize> for BoundedVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}
