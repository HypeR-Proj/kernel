// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Resident page owners for fault-free copies across an I/O trust boundary.

use super::super::contract::MemoryCharge;
use super::*;

/// Retains exactly the readable pages selected during user-range preparation.
/// Mapping replacement and file-cache eviction cannot redirect a later copy.
pub(crate) struct PinnedReadRange<B: PageBackend, A: MemoryAccount> {
    pages: Vec<PageRef<B, A>>,
    backend: B,
    offset: u64,
    length: usize,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> MappingObject<B, A> {
    pub(crate) fn pin_read_range(
        &self,
        offset: u64,
        length: usize,
        account: &A,
    ) -> VmoResult<B, A, PinnedReadRange<B, A>> {
        validate_range(self.size(), offset, length)?;
        let (first, count) = covered_pages(offset, length)?;
        let backend = match self {
            Self::Private(view) => &view.backend,
            Self::Writable(vmo) => &vmo.inner.backend,
            Self::Snapshot(vmo) => &vmo.inner.backend,
            Self::Executable(vmo) => &vmo.inner.backend,
        };
        let bytes = count
            .checked_mul(size_of::<PageRef<B, A>>())
            .ok_or(VmoError::SizeOverflow)?;
        let charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| VmoError::Allocation)?;
        for index in first..first + count {
            pages.push(match self {
                Self::Private(view) => view
                    .pages
                    .get(index)
                    .and_then(Option::as_ref)
                    .map(|page| page.owner.clone())
                    .ok_or(VmoError::BackingNotResident)?,
                Self::Writable(vmo) => page_ref(&vmo.inner, index)?,
                Self::Snapshot(vmo) => page_ref(&vmo.inner, index)?,
                Self::Executable(vmo) => page_ref(&vmo.inner, index)?,
            });
        }
        Ok(PinnedReadRange {
            pages,
            backend: backend.clone(),
            offset,
            length,
            _charge: charge,
        })
    }
}

impl<B: PageBackend, A: MemoryAccount> PinnedReadRange<B, A> {
    pub(crate) fn read_exposed(&self, offset: u64, output: &mut [u8]) -> VmoResult<B, A, ()> {
        let relative = offset
            .checked_sub(self.offset)
            .ok_or(VmoError::InvalidRange)?;
        validate_range(self.length as u64, relative, output.len())?;
        let first = (self.offset / PAGE_SIZE) as usize;
        visit_chunks(
            offset,
            output.len(),
            |index, page_offset, target, length| {
                let page = self
                    .pages
                    .get(index.checked_sub(first).ok_or(VmoError::InvalidRange)?)
                    .ok_or(VmoError::InvalidRange)?;
                page.with(|owned| {
                    self.backend
                        .read_exposed(
                            &owned.page,
                            page_offset,
                            &mut output[target..target + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )
    }

    pub(crate) fn copy_into(
        &self,
        source_offset: u64,
        destination: &WritableVmo<B, A>,
        destination_offset: u64,
        length: usize,
    ) -> VmoResult<B, A, ()> {
        validate_range(self.length as u64, source_offset, length)?;
        validate_range(destination.size(), destination_offset, length)?;
        let absolute = self
            .offset
            .checked_add(source_offset)
            .ok_or(VmoError::SizeOverflow)?;
        let first = usize::try_from(self.offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        copy_ranges(
            &self.backend,
            absolute,
            destination,
            destination_offset,
            length,
            |index| {
                self.pages
                    .get(index.checked_sub(first).ok_or(VmoError::InvalidRange)?)
                    .cloned()
                    .ok_or(VmoError::InvalidRange)
            },
        )
    }
}

impl<B: PageBackend, A: MemoryAccount> WritableVmo<B, A> {
    /// Copies already resident external memory without allocating or faulting.
    /// The caller's request ownership determines when the destination is ready
    /// for a consumer; neither byte range becomes a Rust reference.
    pub(crate) fn copy_from_exposed_vmo(
        &self,
        source: &Self,
        source_offset: u64,
        destination_offset: u64,
        length: usize,
    ) -> VmoResult<B, A, ()> {
        validate_range(source.size(), source_offset, length)?;
        validate_range(self.size(), destination_offset, length)?;
        copy_ranges(
            &source.inner.backend,
            source_offset,
            self,
            destination_offset,
            length,
            |index| page_ref(&source.inner, index),
        )
    }
}

fn copy_ranges<B: PageBackend, A: MemoryAccount>(
    backend: &B,
    source_offset: u64,
    destination: &WritableVmo<B, A>,
    destination_offset: u64,
    length: usize,
    source_page: impl Fn(usize) -> VmoResult<B, A, PageRef<B, A>>,
) -> VmoResult<B, A, ()> {
    let mut copied = 0;
    while copied < length {
        let source_offset = source_offset + copied as u64;
        let destination_offset = destination_offset + copied as u64;
        let source_byte = (source_offset % PAGE_SIZE) as usize;
        let destination_byte = (destination_offset % PAGE_SIZE) as usize;
        let chunk = (length - copied)
            .min(PAGE_SIZE as usize - source_byte)
            .min(PAGE_SIZE as usize - destination_byte);
        let source = source_page((source_offset / PAGE_SIZE) as usize)?;
        let destination = page_ref(
            &destination.inner,
            (destination_offset / PAGE_SIZE) as usize,
        )?;
        copy_page_pair::<B, A>(
            backend,
            &source,
            source_byte,
            &destination,
            destination_byte,
            chunk,
        )?;
        copied += chunk;
    }
    Ok(())
}

fn copy_page_pair<B: PageBackend, A: MemoryAccount>(
    backend: &B,
    source: &PageRef<B, A>,
    source_offset: usize,
    destination: &PageRef<B, A>,
    destination_offset: usize,
    length: usize,
) -> VmoResult<B, A, ()> {
    let source_key = core::ptr::from_ref(&**source).addr();
    let destination_key = core::ptr::from_ref(&**destination).addr();
    if source_key == destination_key {
        return Err(VmoError::InvalidRange);
    }
    // A reverse copy on another CPU must take the same page-owner lock first.
    // Allocation, mapping resolution and peer completion happen outside these
    // two short leaf locks. The backend also rejects physical aliasing between
    // distinct owners (for example two retained views of one cached page).
    let copy = |source: &OwnedPage<B::Page, A::Charge>,
                destination: &mut OwnedPage<B::Page, A::Charge>| {
        backend
            .copy_exposed(
                &source.page,
                source_offset,
                &mut destination.page,
                destination_offset,
                length,
            )
            .map_err(VmoError::Backend)
    };
    if source_key < destination_key {
        source.with(|source| destination.with(|destination| copy(source, destination)))
    } else {
        destination.with(|destination| source.with(|source| copy(source, destination)))
    }
}
