// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-write edits of 4 KiB, 512-entry translation hierarchies.
//! Architecture adapters encode entries; storage retains shared pages until
//! acknowledged retirement. Only paths touched by an edit become writable.

use super::{PAGE_SIZE, PhysicalAddress};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Storage,
    InvalidRange,
    InvalidEntry,
}

/// Storage for an unpublished candidate. `writable` preserves all old readers
/// and copies a shared page at most once. Discard removes only this candidate's
/// ownership, never the published hierarchy's owner. On error the caller must
/// abandon the whole candidate, including any partially edited path.
pub trait Storage {
    const LEVELS: usize;
    fn read(&self, table: PhysicalAddress, slot: usize) -> Result<u64, Error>;
    fn write(&mut self, table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error>;
    fn writable(&mut self, table: Option<PhysicalAddress>) -> Result<PhysicalAddress, Error>;
    fn discard(&mut self, table: PhysicalAddress) -> Result<(), Error>;
    /// Accept only empty entries or child-table entries (no block mappings).
    fn child(entry: u64) -> Result<Option<PhysicalAddress>, Error>;
    fn link(table: PhysicalAddress) -> u64;
}

pub struct Editor<'a, S: Storage> {
    root: PhysicalAddress,
    storage: &'a mut S,
}

impl<'a, S: Storage> Editor<'a, S> {
    pub fn new(root: PhysicalAddress, storage: &'a mut S) -> Result<Self, Error> {
        if !(2..=4).contains(&S::LEVELS) {
            return Err(Error::InvalidRange);
        }
        Ok(Self { root, storage })
    }

    pub const fn root(&self) -> PhysicalAddress {
        self.root
    }

    /// Sets or removes one leaf; a zero descriptor removes it. A no-op does
    /// not copy tables. Empty intermediate tables are pruned immediately.
    pub fn set(&mut self, address: u64, descriptor: u64) -> Result<(), Error> {
        self.validate(address, PAGE_SIZE)?;
        let root = self.set_node(Some(self.root), 0, address, descriptor)?;
        self.root = match root {
            Some(root) => root,
            None => self.storage.writable(None)?,
        };
        Ok(())
    }

    pub fn clear(&mut self, address: u64, length: u64) -> Result<(), Error> {
        self.validate(address, length)?;
        if length == 0 {
            return Ok(());
        }
        let root = self.clear_node(self.root, 0, 0, address, address + length)?;
        self.root = match root {
            Some(root) => root,
            None => self.storage.writable(None)?,
        };
        Ok(())
    }

    fn validate(&self, address: u64, length: u64) -> Result<(), Error> {
        let limit = 1u64 << (12 + 9 * S::LEVELS);
        if !address.is_multiple_of(PAGE_SIZE)
            || !length.is_multiple_of(PAGE_SIZE)
            || address.checked_add(length).is_none_or(|end| end > limit)
        {
            return Err(Error::InvalidRange);
        }
        Ok(())
    }

    fn set_node(
        &mut self,
        table: Option<PhysicalAddress>,
        level: usize,
        address: u64,
        descriptor: u64,
    ) -> Result<Option<PhysicalAddress>, Error> {
        let shift = 12 + 9 * (S::LEVELS - level - 1);
        let slot = ((address >> shift) & 511) as usize;
        let old = match table {
            Some(table) => self.storage.read(table, slot)?,
            None => 0,
        };
        let next = if level + 1 == S::LEVELS {
            descriptor
        } else {
            let child = self.set_node(S::child(old)?, level + 1, address, descriptor)?;
            child.map_or(0, S::link)
        };
        if old == next {
            return Ok(table);
        }
        let writable = self.storage.writable(table)?;
        self.storage.write(writable, slot, next)?;
        self.prune(writable, next == 0)
    }

    fn clear_node(
        &mut self,
        table: PhysicalAddress,
        level: usize,
        base: u64,
        start: u64,
        end: u64,
    ) -> Result<Option<PhysicalAddress>, Error> {
        let span = 1u64 << (12 + 9 * (S::LEVELS - level));
        if start <= base && end >= base + span {
            self.discard_tree(table, level)?;
            return Ok(None);
        }
        let step = span / 512;
        let first = ((start.saturating_sub(base)) / step).min(512) as usize;
        let last = (end.saturating_sub(base).div_ceil(step)).min(512) as usize;
        let mut writable = table;
        let mut changed = false;
        for slot in first..last {
            let old = self.storage.read(writable, slot)?;
            if old == 0 {
                continue;
            }
            let next = if level + 1 == S::LEVELS {
                0
            } else {
                let child = S::child(old)?.ok_or(Error::InvalidEntry)?;
                self.clear_node(child, level + 1, base + slot as u64 * step, start, end)?
                    .map_or(0, S::link)
            };
            if old != next {
                writable = self.storage.writable(Some(writable))?;
                self.storage.write(writable, slot, next)?;
                changed = true;
            }
        }
        self.prune(writable, changed)
    }

    fn prune(
        &mut self,
        table: PhysicalAddress,
        check: bool,
    ) -> Result<Option<PhysicalAddress>, Error> {
        if check {
            for slot in 0..512 {
                if self.storage.read(table, slot)? != 0 {
                    return Ok(Some(table));
                }
            }
            self.storage.discard(table)?;
            Ok(None)
        } else {
            Ok(Some(table))
        }
    }

    fn discard_tree(&mut self, table: PhysicalAddress, level: usize) -> Result<(), Error> {
        if level + 1 < S::LEVELS {
            for slot in 0..512 {
                if let Some(child) = S::child(self.storage.read(table, slot)?)? {
                    self.discard_tree(child, level + 1)?;
                }
            }
        }
        self.storage.discard(table)
    }
}

/// Physical page ownership callback used by architecture-specific editors.
/// The unsafe HAL preparation boundary requires coherent, retained pages.
pub trait Pages {
    fn writable(&mut self, old: Option<PhysicalAddress>) -> Option<PhysicalAddress>;
    fn discard(&mut self, table: PhysicalAddress) -> bool;
}
