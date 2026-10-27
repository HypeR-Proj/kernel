// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared ownership of immutable table pages, with an unpublished edit epoch.
//! The persistent index owns exactly the candidate's reachable tables, not a
//! chain of prior roots. A retired mapping cannot retain unrelated old tables.

use super::super::index::{self, Entry, Index};
use super::{
    DomainAccount, DomainCharge, Error, FallibleArc, MemoryAccount, MemoryCharge, PAGE_SIZE,
    PageBlock,
};
use crate::mm::allocator::heap::PageOwner;
use hyper::mm::{PhysicalAddress, table_edit::Pages};

struct TablePage {
    epoch: u64,
    block: PageBlock,
    _charge: DomainCharge,
}

#[derive(Clone)]
struct Record(FallibleArc<TablePage>);
impl Entry for Record {
    type Key = u64;
    type Weight = u64;
    fn key(&self) -> u64 {
        self.0.block.physical().get()
    }
}

pub(super) struct TablePagePool {
    pages: Index<Record, DomainAccount>,
    epoch: u64,
    account: DomainAccount,
    pub(super) error: Option<Error>,
}

impl TablePagePool {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn len(&self) -> usize {
        self.pages.len()
    }
    pub(super) fn empty(epoch: u64, account: DomainAccount) -> Self {
        Self {
            pages: Index::new(),
            epoch,
            account,
            error: None,
        }
    }

    pub(super) fn fork(&self, epoch: u64) -> Self {
        Self {
            pages: self.pages.clone(),
            epoch,
            account: self.account.clone(),
            error: None,
        }
    }

    pub(super) fn allocate(&mut self, order: usize) -> Option<PhysicalAddress> {
        if order != 0 {
            self.error = Some(Error::Unsupported);
            return None;
        }
        self.writable(None)
    }

    fn prepare(&mut self, old: Option<PhysicalAddress>) -> Result<PhysicalAddress, Error> {
        if let Some(old) = old {
            let record = self.pages.get(old.get()).ok_or(Error::Unsupported)?;
            if record.0.epoch == self.epoch {
                return Ok(old);
            }
        }
        let charge = self
            .account
            .try_charge(MemoryCharge {
                committed_pages: 1,
                pinned_pages: 1,
                kernel_bytes: FallibleArc::<TablePage>::allocation_size() as u64,
                ..MemoryCharge::default()
            })
            .map_err(Error::Resource)?;
        let block = PageBlock::allocate_for(0, PageOwner::PageTable).map_err(Error::Page)?;
        let address =
            crate::mm::memory::linear_address(block.physical().get()).ok_or(Error::Unsupported)?;
        let destination = core::ptr::with_exposed_provenance_mut::<u8>(address);
        match old {
            Some(old) => {
                let source =
                    crate::mm::memory::linear_address(old.get()).ok_or(Error::Unsupported)?;
                // SAFETY: Both full pages are retained by local/index owners.
                // The old page is immutable (hardware AF/dirty bits are already
                // set); the new PageBlock is exclusive and cannot overlap it.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::with_exposed_provenance::<u8>(source),
                        destination,
                        PAGE_SIZE as usize,
                    )
                };
            }
            None => {
                // SAFETY: The new block exclusively owns the full linear page.
                unsafe { destination.write_bytes(0, PAGE_SIZE as usize) };
            }
        }
        let physical = block.physical();
        let record = Record(
            FallibleArc::try_new(TablePage {
                epoch: self.epoch,
                block,
                _charge: charge,
            })
            .map_err(|_| Error::Allocation)?,
        );
        let mut next = self
            .pages
            .insert(record, &self.account)
            .map_err(index_error)?;
        if let Some(old) = old {
            next = next.remove(old.get(), &self.account).map_err(index_error)?;
        }
        self.pages = next;
        Ok(physical)
    }
}

impl Pages for TablePagePool {
    fn writable(&mut self, old: Option<PhysicalAddress>) -> Option<PhysicalAddress> {
        if self.error.is_some() {
            return None;
        }
        match self.prepare(old) {
            Ok(page) => Some(page),
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }
    fn discard(&mut self, table: PhysicalAddress) -> bool {
        if self.error.is_some() {
            return false;
        }
        if self.pages.get(table.get()).is_none() {
            self.error = Some(Error::Unsupported);
            return false;
        }
        match self.pages.remove(table.get(), &self.account) {
            Ok(next) => {
                self.pages = next;
                true
            }
            Err(error) => {
                self.error = Some(index_error(error));
                false
            }
        }
    }
}

fn index_error(error: index::Error<super::ResourceError>) -> Error {
    match error {
        index::Error::Allocation => Error::Allocation,
        index::Error::Account(error) => Error::Resource(error),
    }
}
