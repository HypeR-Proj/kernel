// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable translation descriptions over file-owned mutable physical pages.

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::{FallibleArc, PAGE_SIZE, PhysicalAddress};

use super::super::index::{Entry, Index};
use super::super::{SharedAdmission, SharedAlias, SharedFileError, SharedPage, SharedSource};
use super::{MemoryAccount, MemoryCharge, PageBackend, VmoError, VmoResult, map_allocation};

pub(crate) enum SharedChangeGuard<B: PageBackend> {
    Install {
        guard: <B::SharedPage as SharedPage>::Install,
        _alias: FallibleArc<Box<dyn SharedAlias>>,
    },
    Mutation(<B::SharedPage as SharedPage>::Mutation),
}

impl<B: PageBackend> SharedChangeGuard<B> {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Install { guard, .. } => guard.valid(),
            Self::Mutation(guard) => guard.valid(),
        }
    }
}

pub(crate) fn shared_error<B: PageBackend, A: MemoryAccount>(
    error: SharedFileError,
) -> VmoError<B::Error, A::Error> {
    match error {
        SharedFileError::Busy => VmoError::Busy,
        SharedFileError::Allocation => VmoError::Allocation,
        SharedFileError::Unavailable => VmoError::BackingIo,
        SharedFileError::InvalidRange | SharedFileError::Unsupported => VmoError::InvalidRange,
    }
}

struct SharedOrigin<B: PageBackend, A: MemoryAccount> {
    source: B::SharedSource,
    alias: super::UserMemoryLock<Option<FallibleArc<Box<dyn SharedAlias>>>>,
    live: AtomicBool,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> SharedOrigin<B, A> {
    fn retire(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.source.retire();
            let retired = self.alias.with(Option::take);
            drop(retired);
        }
    }
}

impl<B: PageBackend, A: MemoryAccount> Drop for SharedOrigin<B, A> {
    fn drop(&mut self) {
        self.retire();
    }
}

pub(crate) struct SharedResident<B: PageBackend> {
    index: u64,
    pub(crate) page: B::SharedPage,
    pub(crate) write_epoch: Option<u64>,
}

impl<B: PageBackend> Clone for SharedResident<B> {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            page: self.page.clone(),
            write_epoch: self.write_epoch,
        }
    }
}

impl<B: PageBackend> Entry for SharedResident<B> {
    type Key = u64;
    type Weight = u64;
    fn key(&self) -> u64 {
        self.index
    }
}
fn index_error<B: PageBackend, A: MemoryAccount>(
    error: super::super::index::Error<A::Error>,
) -> VmoError<B::Error, A::Error> {
    match error {
        super::super::index::Error::Account(error) => VmoError::Account(error),
        super::super::index::Error::Allocation => VmoError::Allocation,
    }
}

pub(crate) struct SharedFileView<B: PageBackend, A: MemoryAccount> {
    origin: FallibleArc<SharedOrigin<B, A>>,
    source_offset: u64,
    size: u64,
    pages: Index<SharedResident<B>, A>,
    pub(super) backend: B,
    account: A,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> SharedFileView<B, A> {
    pub(crate) fn try_new(
        source: B::SharedSource,
        alias: FallibleArc<Box<dyn SharedAlias>>,
        source_offset: u64,
        size: u64,
        backend: B,
        account: A,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        if size == 0
            || !size.is_multiple_of(PAGE_SIZE)
            || !source_offset.is_multiple_of(PAGE_SIZE)
            || source_offset
                .checked_add(size)
                .is_none_or(|end| end > source.size())
        {
            return Err(VmoError::InvalidRange);
        }
        let charge = Self::charge(&account)?;
        let origin_charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: FallibleArc::<SharedOrigin<B, A>>::allocation_size() as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)?;
        let origin = FallibleArc::try_new(SharedOrigin {
            source,
            alias: super::UserMemoryLock::new(Some(alias)),
            live: AtomicBool::new(true),
            _charge: origin_charge,
        })
        .map_err(map_allocation)?;
        let pages = Index::new();
        FallibleArc::try_new(Self {
            origin,
            source_offset,
            size,
            pages,
            backend,
            account,
            _charge: charge,
        })
        .map_err(map_allocation)
    }

    fn charge(account: &A) -> VmoResult<B, A, A::Charge> {
        let bytes = FallibleArc::<Self>::allocation_size();
        account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)
    }

    fn clone_version(&self) -> VmoResult<B, A, Self> {
        let charge = Self::charge(&self.account)?;
        let pages = self.pages.clone();
        Ok(Self {
            origin: self.origin.clone(),
            source_offset: self.source_offset,
            size: self.size,
            pages,
            backend: self.backend.clone(),
            account: self.account.clone(),
            _charge: charge,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }
    pub(crate) fn source_offset(&self) -> u64 {
        self.source_offset
    }
    pub(crate) fn identity(&self) -> (u64, u64) {
        self.origin.source.identity()
    }
    pub(crate) fn same_origin(&self, other: &Self) -> bool {
        core::ptr::eq(&*self.origin, &*other.origin)
    }
    pub(crate) fn retire(&self) {
        self.origin.retire();
    }
    pub(crate) fn retain_sync(&self) -> VmoResult<B, A, B::SharedSource> {
        self.require_live()?;
        self.origin
            .source
            .retain_sync()
            .map_err(shared_error::<B, A>)
    }
    fn require_live(&self) -> VmoResult<B, A, ()> {
        self.origin
            .live
            .load(Ordering::Acquire)
            .then_some(())
            .ok_or(VmoError::BackingIo)
    }
    pub(crate) fn has_page(&self, index: usize) -> bool {
        self.pages.get(index as u64).is_some()
    }
    pub(crate) fn resident_offsets(&self) -> impl Iterator<Item = u64> + '_ {
        self.pages.iter().map(|page| page.index * PAGE_SIZE)
    }
    pub(crate) fn has_resident_pages(&self) -> bool {
        !self.pages.is_empty()
    }

    pub(crate) fn populate(
        &self,
        first: usize,
        count: usize,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        self.require_live()?;
        let end = first
            .checked_add(count)
            .filter(|end| *end as u64 <= self.size / PAGE_SIZE)
            .ok_or(VmoError::InvalidRange)?;
        let mut next = self.clone_version()?;
        for index in first..end {
            if next.pages.get(index as u64).is_some() {
                continue;
            }
            let page = self
                .origin
                .source
                .page(self.source_offset + index as u64 * PAGE_SIZE)
                .map_err(shared_error::<B, A>)?;
            next.pages = next
                .pages
                .insert(
                    SharedResident {
                        index: index as u64,
                        page,
                        write_epoch: None,
                    },
                    &self.account,
                )
                .map_err(index_error::<B, A>)?;
        }
        FallibleArc::try_new(next).map_err(map_allocation)
    }

    /// Population may reclaim or wait. Admit installation only after every
    /// source page in the complete address-space transaction has been loaded.
    pub(crate) fn admit_population(
        &self,
        previous: &Self,
        first: usize,
        count: usize,
        guards: &mut Vec<SharedChangeGuard<B>>,
    ) -> VmoResult<B, A, ()> {
        self.require_live()?;
        let alias = self
            .origin
            .alias
            .with(|alias| alias.clone())
            .ok_or(VmoError::BackingIo)?;
        let end = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
        guards
            .try_reserve(count)
            .map_err(|_| VmoError::Allocation)?;
        for index in first..end {
            if previous.has_page(index) {
                continue;
            }
            let page = self.page(index)?;
            let guard = page.try_begin_install().map_err(shared_error::<B, A>)?;
            page.register_alias(alias.downgrade())
                .map_err(shared_error::<B, A>)?;
            guards.push(SharedChangeGuard::Install {
                guard,
                _alias: alias.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn grant_write(
        &self,
        index: usize,
        guards: &mut Vec<SharedChangeGuard<B>>,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        self.require_live()?;
        let mut next = self.clone_version()?;
        let mut resident = next
            .pages
            .get(index as u64)
            .cloned()
            .ok_or(VmoError::BackingNotResident)?;
        guards.try_reserve(1).map_err(|_| VmoError::Allocation)?;
        let guard = resident
            .page
            .try_begin_write()
            .map_err(shared_error::<B, A>)?;
        resident.write_epoch = Some(guard.epoch());
        guards.push(SharedChangeGuard::Mutation(guard));
        next.pages = next
            .pages
            .insert(resident, &self.account)
            .map_err(index_error::<B, A>)?;
        FallibleArc::try_new(next).map_err(map_allocation)
    }

    /// Prepare the allocation-free emergency target. It owns mapping authority
    /// but no resident frames; normal writeback uses targeted withdrawal below.
    pub(crate) fn without_residents(&self) -> VmoResult<B, A, FallibleArc<Self>> {
        let mut next = self.clone_version()?;
        next.pages = Index::new();
        FallibleArc::try_new(next).map_err(map_allocation)
    }

    /// Withdraw every occurrence of this canonical page in this view. The
    /// caller keeps page admission closed through the hardware acknowledgement.
    /// Check the live cut epoch only for selection, never to reinterpret an old
    /// immutable mapping's permissions while enumerating its translation diff.
    pub(crate) fn withdraw_page(
        &self,
        page_id: u64,
        epoch: u64,
        all: bool,
    ) -> VmoResult<B, A, Option<FallibleArc<Self>>> {
        let matches = |resident: &SharedResident<B>| {
            resident.page.identity() == page_id
                && resident.page.epoch() == epoch
                && (all || resident.write_epoch.is_some_and(|grant| grant <= epoch))
        };
        if !self.pages.iter().any(matches) {
            return Ok(None);
        }
        let mut next = self.clone_version()?;
        for old in self.pages.iter().filter(|page| matches(page)) {
            next.pages = if all {
                next.pages.remove(old.index, &self.account)
            } else {
                let mut page = old.clone();
                page.write_epoch = None;
                next.pages.insert(page, &self.account)
            }
            .map_err(index_error::<B, A>)?;
        }
        FallibleArc::try_new(next).map(Some).map_err(map_allocation)
    }

    pub(crate) fn physical_page(&self, offset: u64) -> VmoResult<B, A, PhysicalAddress> {
        if !offset.is_multiple_of(PAGE_SIZE) {
            return Err(VmoError::InvalidRange);
        }
        self.pages
            .get(offset / PAGE_SIZE)
            .map(|p| p.page.physical_address())
            .ok_or(VmoError::BackingNotResident)
    }
    pub(crate) fn writable(&self, offset: u64, length: usize) -> VmoResult<B, A, bool> {
        super::validate_range(self.size, offset, length)?;
        let (first, count) = super::covered_pages(offset, length)?;
        Ok((first..first + count).all(|index| {
            self.pages
                .get(index as u64)
                .is_some_and(|page| page.write_epoch.is_some())
        }))
    }
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> VmoResult<B, A, ()> {
        self.require_live()?;
        super::validate_range(self.size, offset, output.len())?;
        super::visit_chunks(offset, output.len(), |index, at, out, len| {
            self.pages
                .get(index as u64)
                .ok_or(VmoError::BackingNotResident)?
                .page
                .read_exposed(at, &mut output[out..out + len])
                .map_err(shared_error::<B, A>)
        })
    }

    pub(crate) fn page(&self, index: usize) -> VmoResult<B, A, &B::SharedPage> {
        self.require_live()?;
        self.pages
            .get(index as u64)
            .map(|p| &p.page)
            .ok_or(VmoError::BackingNotResident)
    }
}
