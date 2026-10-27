// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible vectors whose capacity remains charged through destruction.

use super::allocation_account::StorageBudget;
use alloc::vec::{IntoIter, Vec};
use core::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Allocation,
    Size,
    Budget(E),
}

pub struct BudgetedVec<T, B: StorageBudget> {
    // Field order is part of accounting: free storage before releasing charge.
    values: Vec<T>,
    charge: Option<B::Charge>,
    budget: B,
}
impl<T, B: StorageBudget> BudgetedVec<T, B> {
    pub const fn new(budget: B) -> Self {
        Self {
            values: Vec::new(),
            charge: None,
            budget,
        }
    }
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), Error<B::Error>> {
        let required = self
            .values
            .len()
            .checked_add(additional)
            .ok_or(Error::Size)?;
        if required <= self.values.capacity() {
            return Ok(());
        }
        self.grow(required.checked_next_power_of_two().ok_or(Error::Size)?)
    }
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), Error<B::Error>> {
        let required = self
            .values
            .len()
            .checked_add(additional)
            .ok_or(Error::Size)?;
        if required <= self.values.capacity() {
            return Ok(());
        }
        self.grow(required)
    }
    fn grow(&mut self, capacity: usize) -> Result<(), Error<B::Error>> {
        let bytes = capacity
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(Error::Size)?;
        let charge = self.budget.reserve(bytes).map_err(Error::Budget)?;
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        // Global allocation reports the requested layout as usable extent.
        // Reject an allocator contract change rather than undercharging it.
        if replacement.capacity() != capacity {
            return Err(Error::Allocation);
        }
        replacement.append(&mut self.values);
        let old = core::mem::replace(&mut self.values, replacement);
        drop(old);
        self.charge = Some(charge);
        Ok(())
    }
    pub fn push(&mut self, value: T) -> Result<(), Error<B::Error>> {
        self.try_reserve(1)?;
        self.values.push(value);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop()
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }
    pub fn resize(&mut self, length: usize, value: T) -> Result<(), Error<B::Error>>
    where
        T: Clone,
    {
        self.try_reserve_exact(length.saturating_sub(self.values.len()))?;
        self.values.resize(length, value);
        Ok(())
    }
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<(), Error<B::Error>>
    where
        T: Copy,
    {
        self.try_reserve(values.len())?;
        self.values.extend_from_slice(values);
        Ok(())
    }
}
impl<T, B: StorageBudget> Deref for BudgetedVec<T, B> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.values
    }
}
impl<T, B: StorageBudget> DerefMut for BudgetedVec<T, B> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.values
    }
}

pub struct BudgetedIntoIter<T, C> {
    values: IntoIter<T>,
    _charge: Option<C>,
}
impl<T, B: StorageBudget> IntoIterator for BudgetedVec<T, B> {
    type Item = T;
    type IntoIter = BudgetedIntoIter<T, B::Charge>;
    fn into_iter(self) -> Self::IntoIter {
        BudgetedIntoIter {
            values: self.values.into_iter(),
            _charge: self.charge,
        }
    }
}
impl<T, C> Iterator for BudgetedIntoIter<T, C> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.values.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.values.size_hint()
    }
}
impl<T, C> ExactSizeIterator for BudgetedIntoIter<T, C> {}
impl<T, C> DoubleEndedIterator for BudgetedIntoIter<T, C> {
    fn next_back(&mut self) -> Option<T> {
        self.values.next_back()
    }
}
