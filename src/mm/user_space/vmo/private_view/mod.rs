// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable private mapping versions and unpublished executable construction.

use super::{
    ExecutableProvenance, MemoryAccount, MemoryCharge, PageBackend, PageRef, PrivateMappingMode,
    SnapshotVmo, VmoError, VmoResult, allocate_page, covered_pages, map_allocation, page_count,
    page_ref, validate_range, visit_chunks,
};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::{FallibleArc, PAGE_SIZE};

pub(super) struct PrivatePage<B: PageBackend, A: MemoryAccount> {
    pub(super) owner: PageRef<B, A>,
    writable: bool,
}
impl<B: PageBackend, A: MemoryAccount> Clone for PrivatePage<B, A> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            writable: self.writable,
        }
    }
}

mod file;
use file::FileOrigin;

/// One immutable physical-page version of an address-space-owned private view.
/// Replacing this value never changes pages in an older mapping or pin.
pub(crate) struct PrivateView<B: PageBackend, A: MemoryAccount> {
    file: Option<FileOrigin<B, A>>,
    pub(super) pages: Vec<Option<PrivatePage<B, A>>>,
    pub(super) backend: B,
    account: A,
    _charge: A::Charge,
    pub(super) executable: bool,
    instructions_published: AtomicBool,
}

impl<B: PageBackend, A: MemoryAccount> PrivateView<B, A> {
    /// Only materialized private pages qualify. Clean file/source/zero pages
    /// may be shared with other address spaces and must not become dump pins.
    pub(super) fn retain_crash_page(&self, index: usize) -> Option<super::CrashPage<B, A>> {
        let page = self.pages.get(index)?.as_ref()?;
        if !page.writable {
            return None;
        }
        super::CrashPage::try_new(page.owner.clone(), self.backend.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        source: SnapshotVmo<B, A>,
        size: u64,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        mode: PrivateMappingMode,
        backend: B,
        account: A,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        let count = page_count(size)?;
        let end = data_offset
            .checked_add(source_length)
            .ok_or(VmoError::SizeOverflow)?;
        if data_offset >= PAGE_SIZE
            || !source_offset.is_multiple_of(PAGE_SIZE)
            || end > size
            || (source_length != 0
                && source_offset
                    .checked_add(end)
                    .ok_or(VmoError::SizeOverflow)?
                    > source.size())
        {
            return Err(VmoError::InvalidRange);
        }
        let charge = Self::charge(&account, count)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| VmoError::Allocation)?;
        let mut zero: Option<PageRef<B, A>> = None;
        for index in 0..count {
            let start = index as u64 * PAGE_SIZE;
            let full = start >= data_offset && start + PAGE_SIZE <= end;
            let owner = if full && mode == PrivateMappingMode::CopyOnWrite {
                page_ref(
                    &source.inner,
                    usize::try_from((source_offset + start) / PAGE_SIZE)
                        .map_err(|_| VmoError::SizeOverflow)?,
                )?
            } else if mode == PrivateMappingMode::CopyOnWrite
                && (start >= end || start + PAGE_SIZE <= data_offset)
            {
                match &zero {
                    Some(page) => page.clone(),
                    None => {
                        let page = allocate_page(&backend, &account)?;
                        zero = Some(page.clone());
                        page
                    }
                }
            } else {
                let page = allocate_page(&backend, &account)?;
                let first = start.max(data_offset);
                let last = (start + PAGE_SIZE).min(end);
                if first < last {
                    let source_page = page_ref(
                        &source.inner,
                        usize::try_from((source_offset + first) / PAGE_SIZE)
                            .map_err(|_| VmoError::SizeOverflow)?,
                    )?;
                    // The immutable source and newly allocated destination are
                    // distinct. The destination has never escaped, so nesting
                    // its lock cannot create a lock cycle. Zeroed bytes outside
                    // this exact range remain undisclosed boundary/BSS padding.
                    source_page.with(|owned| {
                        page.with(|destination| {
                            backend
                                .copy_owned(
                                    &owned.page,
                                    (first - start) as usize,
                                    &mut destination.page,
                                    (first - start) as usize,
                                    (last - first) as usize,
                                )
                                .map_err(VmoError::Backend)
                        })
                    })?;
                }
                page
            };
            // Boundary pages are private already. Shared zero/source pages
            // must fault before any machine or kernel writer gains access.
            let writable = mode == PrivateMappingMode::Eager
                || (!full && start < end && start + PAGE_SIZE > data_offset);
            pages.push(Some(PrivatePage { owner, writable }));
        }
        FallibleArc::try_new(Self {
            file: None,
            pages,
            backend,
            account,
            _charge: charge,
            executable: false,
            instructions_published: AtomicBool::new(false),
        })
        .map_err(map_allocation)
    }

    fn charge(account: &A, count: usize) -> VmoResult<B, A, A::Charge> {
        let bytes = count
            .checked_mul(size_of::<Option<PrivatePage<B, A>>>())
            .and_then(|bytes| bytes.checked_add(FallibleArc::<Self>::allocation_size()))
            .ok_or(VmoError::SizeOverflow)?;
        account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)
    }

    pub(crate) fn size(&self) -> u64 {
        self.pages.len() as u64 * PAGE_SIZE
    }
    pub(crate) fn is_writable(&self, index: usize) -> bool {
        !self.executable
            && self
                .pages
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|page| page.writable)
    }
    pub(crate) fn materialize(
        &self,
        first: usize,
        count: usize,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        if self.executable {
            return Err(VmoError::InvalidRange);
        }
        let end = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
        if end > self.pages.len() {
            return Err(VmoError::InvalidRange);
        }
        let charge = Self::charge(&self.account, self.pages.len())?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(self.pages.len())
            .map_err(|_| VmoError::Allocation)?;
        for (index, slot) in self.pages.iter().enumerate() {
            let Some(original) = slot else {
                if index >= first && index < end {
                    return Err(VmoError::BackingNotResident);
                }
                pages.push(None);
                continue;
            };
            if index < first || index >= end || original.writable {
                pages.push(Some(original.clone()));
                continue;
            }
            let owner = allocate_page(&self.backend, &self.account)?;
            // Only this transaction can reach the new destination. Taking its
            // lock under the original page lock cannot form a lock cycle, and
            // a failed copy drops the unpublished page without changing self.
            original.owner.with(|owned| {
                owner.with(|destination| {
                    self.backend
                        .copy_owned(&owned.page, 0, &mut destination.page, 0, PAGE_SIZE as usize)
                        .map_err(VmoError::Backend)
                })
            })?;
            pages.push(Some(PrivatePage {
                owner,
                writable: true,
            }));
        }
        FallibleArc::try_new(Self {
            file: self.file.clone(),
            pages,
            backend: self.backend.clone(),
            account: self.account.clone(),
            _charge: charge,
            executable: false,
            instructions_published: AtomicBool::new(false),
        })
        .map_err(map_allocation)
    }

    pub(crate) fn pin_write_range(
        &self,
        offset: u64,
        length: usize,
    ) -> VmoResult<B, A, PinnedPrivateRange<B, A>> {
        validate_range(self.size(), offset, length)?;
        if !self.range_writable(offset, length)? {
            return Err(VmoError::CowRequired);
        }
        let (first, count) = covered_pages(offset, length)?;
        let bytes = count
            .checked_mul(size_of::<PageRef<B, A>>())
            .ok_or(VmoError::SizeOverflow)?;
        let charge = self
            .account
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
            let page = self
                .pages
                .get(index)
                .and_then(Option::as_ref)
                .ok_or(VmoError::InvalidRange)?;
            pages.push(page.owner.clone());
        }
        Ok(PinnedPrivateRange {
            pages,
            backend: self.backend.clone(),
            offset,
            length,
            _charge: charge,
        })
    }

    pub(crate) fn retain_pages(
        &self,
        keep: impl Fn(usize) -> bool,
    ) -> VmoResult<B, A, Option<FallibleArc<Self>>> {
        if !self
            .pages
            .iter()
            .enumerate()
            .any(|(index, page)| page.is_some() && !keep(index))
        {
            return Ok(None);
        }
        let charge = Self::charge(&self.account, self.pages.len())?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(self.pages.len())
            .map_err(|_| VmoError::Allocation)?;
        for (index, page) in self.pages.iter().enumerate() {
            pages.push(if keep(index) { page.clone() } else { None });
        }
        Ok(Some(
            FallibleArc::try_new(Self {
                file: self.file.clone(),
                pages,
                backend: self.backend.clone(),
                account: self.account.clone(),
                _charge: charge,
                executable: self.executable,
                instructions_published: AtomicBool::new(
                    self.instructions_published.load(Ordering::Acquire),
                ),
            })
            .map_err(map_allocation)?,
        ))
    }

    pub(super) fn read(&self, offset: u64, destination: &mut [u8]) -> VmoResult<B, A, ()> {
        validate_range(self.size(), offset, destination.len())?;
        visit_chunks(
            offset,
            destination.len(),
            |index, page_offset, target, length| {
                let page = self
                    .pages
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or(VmoError::BackingNotResident)?;
                page.owner.with(|owned| {
                    self.backend
                        .read_exposed(
                            &owned.page,
                            page_offset,
                            &mut destination[target..target + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )
    }
    pub(super) fn write(&self, offset: u64, source: &[u8]) -> VmoResult<B, A, ()> {
        validate_range(self.size(), offset, source.len())?;
        if !self.range_writable(offset, source.len())? {
            return Err(VmoError::CowRequired);
        }
        visit_chunks(
            offset,
            source.len(),
            |index, page_offset, target, length| {
                let page = self
                    .pages
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or(VmoError::BackingNotResident)?;
                page.owner.with(|owned| {
                    self.backend
                        .write_exposed(
                            &mut owned.page,
                            page_offset,
                            &source[target..target + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )
    }
    pub(super) fn range_writable(&self, offset: u64, length: usize) -> VmoResult<B, A, bool> {
        let (first, count) = covered_pages(offset, length)?;
        Ok((first..first + count).all(|index| self.is_writable(index)))
    }
}

/// Exact writable physical owners retained by a potentially blocking output
/// reservation. No whole private-view version or source snapshot is retained.
pub(crate) struct PinnedPrivateRange<B: PageBackend, A: MemoryAccount> {
    pages: Vec<PageRef<B, A>>,
    backend: B,
    offset: u64,
    length: usize,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> PinnedPrivateRange<B, A> {
    fn validate(&self, offset: u64, length: usize) -> VmoResult<B, A, ()> {
        let relative = offset
            .checked_sub(self.offset)
            .ok_or(VmoError::InvalidRange)?;
        validate_range(self.length as u64, relative, length)
    }
    pub(crate) fn read_exposed(&self, offset: u64, destination: &mut [u8]) -> VmoResult<B, A, ()> {
        self.validate(offset, destination.len())?;
        let first = usize::try_from(self.offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        visit_chunks(
            offset,
            destination.len(),
            |index, page_offset, target, length| {
                let index = index.checked_sub(first).ok_or(VmoError::InvalidRange)?;
                let page = self.pages.get(index).ok_or(VmoError::InvalidRange)?;
                page.with(|owned| {
                    self.backend
                        .read_exposed(
                            &owned.page,
                            page_offset,
                            &mut destination[target..target + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )
    }
    pub(crate) fn write_exposed(&self, offset: u64, source: &[u8]) -> VmoResult<B, A, ()> {
        self.validate(offset, source.len())?;
        let first = usize::try_from(self.offset / PAGE_SIZE).map_err(|_| VmoError::SizeOverflow)?;
        visit_chunks(
            offset,
            source.len(),
            |index, page_offset, target, length| {
                let index = index.checked_sub(first).ok_or(VmoError::InvalidRange)?;
                let page = self.pages.get(index).ok_or(VmoError::InvalidRange)?;
                page.with(|owned| {
                    self.backend
                        .write_exposed(
                            &mut owned.page,
                            page_offset,
                            &source[target..target + length],
                        )
                        .map_err(VmoError::Backend)
                })
            },
        )
    }
}

/// Unpublished private image construction. The view never escapes before
/// finish, so instruction publication cannot race a writable translation or
/// a retained writable version. This builder is deliberately not Clone.
pub(crate) struct PrivateViewBuilder<B: PageBackend, A: MemoryAccount> {
    view: FallibleArc<PrivateView<B, A>>,
}

impl<B: PageBackend, A: MemoryAccount> PrivateViewBuilder<B, A> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        source: SnapshotVmo<B, A>,
        size: u64,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        mode: PrivateMappingMode,
        backend: B,
        account: A,
    ) -> VmoResult<B, A, Self> {
        Ok(Self {
            view: PrivateView::try_new(
                source,
                size,
                source_offset,
                source_length,
                data_offset,
                mode,
                backend,
                account,
            )?,
        })
    }
    pub(crate) fn finish(self) -> FallibleArc<PrivateView<B, A>> {
        self.view
    }
    pub(crate) fn finish_executable(
        self,
        _provenance: &ExecutableProvenance,
        context: &B::InstructionPublicationContext,
    ) -> VmoResult<B, A, FallibleArc<PrivateView<B, A>>> {
        let mut view = self.view.try_into_unique().map_err(|_| VmoError::Busy)?;
        view.backend
            .publish_instruction_pages(context, |visit| {
                for page in view.pages.iter().flatten() {
                    page.owner.with(|owned| visit(&mut owned.page));
                }
            })
            .map_err(VmoError::Backend)?;
        view.executable = true;
        Ok(view.into_shared())
    }
}
