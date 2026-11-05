// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible, page-backed storage for memory-resident files.

use alloc::vec::Vec;
use hyper::mm::AllocationError;

pub use hyper::collections::allocation_account::StorageBudget;

pub(crate) const PAGE_BYTES: usize = hyper::mm::PAGE_SIZE as usize;

/// Exclusive initialized storage; no mapping or DMA alias may mutate the page.
pub(crate) trait Page {
    fn bytes(&self) -> &[u8; PAGE_BYTES];
    fn bytes_mut(&mut self) -> &mut [u8; PAGE_BYTES];
}

pub(crate) trait Storage: StorageBudget {
    type Page: Page;

    /// Payload accounting may be observed separately from index reservations.
    fn reserve_page(&self) -> Result<Self::Charge, Self::Error> {
        self.reserve(PAGE_BYTES)
    }

    /// Return one fully zeroed page. Admission is performed before this call.
    fn allocate_zeroed_page(&self) -> Result<Self::Page, AllocationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error<E> {
    Allocation,
    Size,
    Budget(E),
}

struct OwnedPage<P, C> {
    // Free backing storage before returning its reservation.
    page: P,
    _charge: C,
}

struct Index<P, C> {
    pages: Vec<OwnedPage<P, C>>,
    _charge: Option<C>,
}

impl<P, C> Index<P, C> {
    const fn empty() -> Self {
        Self {
            pages: Vec::new(),
            _charge: None,
        }
    }

    fn reserve<B: StorageBudget<Charge = C>>(
        capacity: usize,
        budget: &B,
    ) -> Result<Self, Error<B::Error>> {
        let bytes = capacity
            .checked_mul(core::mem::size_of::<OwnedPage<P, C>>())
            .ok_or(Error::Size)?;
        let charge = budget.reserve(bytes).map_err(Error::Budget)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        if pages.capacity() != capacity {
            return Err(Error::Allocation);
        }
        Ok(Self {
            pages,
            _charge: Some(charge),
        })
    }
}

/// The caller serializes access. Archive bytes remain borrowed until mutation
/// needs owned storage. Growth never replaces existing payload pages; only the
/// compact index may move. Every fallible preparation precedes publication.
pub(crate) struct FileData<'archive, P, C> {
    archive: &'archive [u8],
    owned: Index<P, C>,
    length: usize,
}

impl<'archive, P: Page, C> FileData<'archive, P, C> {
    pub(crate) const fn borrowed(archive: &'archive [u8]) -> Self {
        Self {
            archive,
            owned: Index::empty(),
            length: archive.len(),
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn archive(&self) -> Option<&'archive [u8]> {
        self.owned.pages.is_empty().then_some(self.archive)
    }

    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> usize {
        let Ok(offset) = usize::try_from(offset) else {
            return 0;
        };
        if offset >= self.length {
            return 0;
        }
        let count = (self.length - offset).min(output.len());
        if let Some(archive) = self.archive() {
            output[..count].copy_from_slice(&archive[offset..offset + count]);
        } else {
            let mut done = 0;
            while done < count {
                let at = offset + done;
                let within = at % PAGE_BYTES;
                let bytes = (PAGE_BYTES - within).min(count - done);
                output[done..done + bytes].copy_from_slice(
                    &self.owned.pages[at / PAGE_BYTES].page.bytes()[within..within + bytes],
                );
                done += bytes;
            }
        }
        count
    }

    pub(crate) fn write<B: Storage<Page = P, Charge = C>>(
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
        self.make_owned(end.max(self.length), budget)?;
        let mut done = 0;
        while done < input.len() {
            let at = offset + done;
            let within = at % PAGE_BYTES;
            let count = (PAGE_BYTES - within).min(input.len() - done);
            self.owned.pages[at / PAGE_BYTES].page.bytes_mut()[within..within + count]
                .copy_from_slice(&input[done..done + count]);
            done += count;
        }
        self.length = self.length.max(end);
        Ok(input.len())
    }

    pub(crate) fn resize<B: Storage<Page = P, Charge = C>>(
        &mut self,
        length: u64,
        budget: &B,
    ) -> Result<(), Error<B::Error>> {
        let length = usize::try_from(length).map_err(|_| Error::Size)?;
        if length == self.length {
            return Ok(());
        }
        if length == 0 {
            self.owned = Index::empty();
            self.archive = &[];
        } else if self.archive().is_some() && length < self.length {
            self.archive = &self.archive[..length];
        } else if length < self.length {
            self.owned.pages.truncate(page_count(length));
            let tail = length % PAGE_BYTES;
            if tail != 0 {
                self.owned.pages[length / PAGE_BYTES].page.bytes_mut()[tail..].fill(0);
            }
        } else {
            self.make_owned(length, budget)?;
        }
        self.length = length;
        Ok(())
    }

    fn make_owned<B: Storage<Page = P, Charge = C>>(
        &mut self,
        required: usize,
        budget: &B,
    ) -> Result<(), Error<B::Error>> {
        let required = page_count(required);
        let existing = self.owned.pages.len();
        if required <= existing {
            return Ok(());
        }

        // Neither an index growth nor a partially allocated suffix may become
        // visible if later admission/allocation fails. The temporary index is
        // charged too, and holds only the new pages, never copies of old data.
        let replacement = if required > self.owned.pages.capacity() {
            Some(Index::reserve(
                required.checked_next_power_of_two().ok_or(Error::Size)?,
                budget,
            )?)
        } else {
            None
        };
        let mut pending = Index::reserve(required - existing, budget)?;
        for number in existing..required {
            let charge = budget.reserve_page().map_err(Error::Budget)?;
            let mut page = budget
                .allocate_zeroed_page()
                .map_err(|_| Error::Allocation)?;
            let offset = number * PAGE_BYTES;
            if offset < self.archive.len() {
                let count = (self.archive.len() - offset).min(PAGE_BYTES);
                page.bytes_mut()[..count].copy_from_slice(&self.archive[offset..offset + count]);
            }
            pending.pages.push(OwnedPage {
                page,
                _charge: charge,
            });
        }
        if let Some(mut replacement) = replacement {
            replacement.pages.append(&mut self.owned.pages);
            self.owned = replacement;
        }
        self.owned.pages.append(&mut pending.pages);
        self.archive = &[];
        Ok(())
    }
}

fn page_count(length: usize) -> usize {
    length / PAGE_BYTES + usize::from(!length.is_multiple_of(PAGE_BYTES))
}
