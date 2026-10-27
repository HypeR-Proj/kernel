// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! UTF-8 strings backed by capacity-accounted vectors.

use super::allocation_account::StorageBudget;
use super::budgeted_vec::BudgetedVec;
pub use super::budgeted_vec::Error;
use core::ops::Deref;

pub struct BudgetedString<B: StorageBudget> {
    bytes: BudgetedVec<u8, B>,
}
impl<B: StorageBudget> BudgetedString<B> {
    pub const fn new(budget: B) -> Self {
        Self {
            bytes: BudgetedVec::new(budget),
        }
    }
    pub fn from_utf8(bytes: BudgetedVec<u8, B>) -> Result<Self, core::str::Utf8Error> {
        core::str::from_utf8(&bytes)?;
        Ok(Self { bytes })
    }
    pub fn from_str(value: &str, budget: B) -> Result<Self, Error<B::Error>> {
        let mut text = Self::new(budget);
        text.try_reserve_exact(value.len())?;
        text.push_str(value)?;
        Ok(text)
    }
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), Error<B::Error>> {
        self.bytes.try_reserve_exact(additional)
    }
    pub fn push_str(&mut self, value: &str) -> Result<(), Error<B::Error>> {
        self.bytes.extend_from_slice(value.as_bytes())
    }
    pub fn push(&mut self, value: char) -> Result<(), Error<B::Error>> {
        let mut bytes = [0; 4];
        self.push_str(value.encode_utf8(&mut bytes))
    }
    pub fn as_str(&self) -> &str {
        // SAFETY: construction starts empty or validates the complete byte buffer.
        // Subsequent mutation can only receive
        // complete UTF-8 strings through push_str/push. No mutable bytes escape.
        unsafe { core::str::from_utf8_unchecked(&self.bytes) }
    }
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}
impl<B: StorageBudget> Deref for BudgetedString<B> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl<B: StorageBudget> core::fmt::Debug for BudgetedString<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.as_str().fmt(f)
    }
}
impl<B: StorageBudget> PartialEq<&str> for BudgetedString<B> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
