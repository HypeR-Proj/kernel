// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible copy-on-write storage for memory-resident files.

use alloc::vec::Vec;

pub use hyper::collections::allocation_account::StorageBudget;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Allocation,
    Size,
    Budget(E),
}

/// The caller serializes access. Archive bytes remain borrowed until a write.
pub struct FileData<'archive, C> {
    archive: &'archive [u8],
    owned: Vec<u8>,
    charge: Option<C>,
}

impl<'archive, C> FileData<'archive, C> {
    pub const fn borrowed(archive: &'archive [u8]) -> Self {
        Self {
            archive,
            owned: Vec::new(),
            charge: None,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        if self.charge.is_some() {
            &self.owned
        } else {
            self.archive
        }
    }

    pub fn archive(&self) -> Option<&'archive [u8]> {
        self.charge.is_none().then_some(self.archive)
    }

    pub fn read(&self, offset: u64, output: &mut [u8]) -> usize {
        let Ok(offset) = usize::try_from(offset) else {
            return 0;
        };
        let Some(source) = self.bytes().get(offset..) else {
            return 0;
        };
        let count = source.len().min(output.len());
        output[..count].copy_from_slice(&source[..count]);
        count
    }

    pub fn write<B: StorageBudget<Charge = C>>(
        &mut self,
        offset: u64,
        input: &[u8],
        budget: &B,
    ) -> Result<usize, Error<B::Error>> {
        if input.is_empty() {
            return Ok(0);
        }
        let offset = usize::try_from(offset).map_err(|_| Error::Size)?;
        let end = offset.checked_add(input.len()).ok_or(Error::Size)?;
        self.make_owned(end.max(self.bytes().len()), budget)?;
        self.owned.resize(end.max(self.owned.len()), 0);
        self.owned[offset..end].copy_from_slice(input);
        Ok(input.len())
    }

    pub fn resize<B: StorageBudget<Charge = C>>(
        &mut self,
        length: u64,
        budget: &B,
    ) -> Result<(), Error<B::Error>> {
        let length = usize::try_from(length).map_err(|_| Error::Size)?;
        if length == self.bytes().len() {
            return Ok(());
        }
        if length == 0 {
            self.archive = &[];
            self.owned = Vec::new();
            self.charge = None;
        } else if self.charge.is_none() && length < self.archive.len() {
            self.archive = &self.archive[..length];
        } else {
            self.make_owned(length, budget)?;
            self.owned.resize(length, 0);
        }
        Ok(())
    }

    fn make_owned<B: StorageBudget<Charge = C>>(
        &mut self,
        required: usize,
        budget: &B,
    ) -> Result<(), Error<B::Error>> {
        if self.charge.is_some() && required <= self.owned.capacity() {
            return Ok(());
        }
        let capacity = required.checked_next_power_of_two().ok_or(Error::Size)?;
        let charge = budget.reserve(capacity).map_err(Error::Budget)?;
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        replacement.extend_from_slice(self.bytes());
        // Both allocations stay charged until the previous buffer is freed.
        self.owned = replacement;
        self.charge = Some(charge);
        self.archive = &[];
        Ok(())
    }
}
