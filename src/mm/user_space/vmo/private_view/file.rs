// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pageable private file contents and protection lifetime.

use super::super::{FileBacking, FilePageSource};
use super::{
    AtomicBool, FallibleArc, MemoryAccount, MemoryCharge, Ordering, PAGE_SIZE, PageBackend,
    PrivatePage, PrivateView, Vec, VmoError, VmoResult, allocate_page, map_allocation, page_count,
};

pub(super) struct FileOrigin<B: PageBackend, A: MemoryAccount> {
    backing: FallibleArc<FileBacking<B, A>>,
    offset: u64,
    length: u64,
    data_offset: u64,
    mode: super::PrivateMappingMode,
}
impl<B: PageBackend, A: MemoryAccount> Clone for FileOrigin<B, A> {
    fn clone(&self) -> Self {
        Self {
            backing: self.backing.clone(),
            offset: self.offset,
            length: self.length,
            data_offset: self.data_offset,
            mode: self.mode,
        }
    }
}

impl<B: PageBackend, A: MemoryAccount> PrivateView<B, A> {
    /// File mappings own only their requested range. Missing clean pages can
    /// be reconstructed from the protected source; private writable pages cannot.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_file(
        source: alloc::boxed::Box<dyn FilePageSource<B>>,
        size: u64,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        executable: bool,
        mode: super::PrivateMappingMode,
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
                    .is_none_or(|end| end > source.size()))
        {
            return Err(VmoError::InvalidRange);
        }
        let backing = FileBacking::try_new(source, &account)?;
        let charge = Self::charge(&account, count)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| VmoError::Allocation)?;
        pages.resize_with(count, || None);
        let empty = Self {
            file: Some(FileOrigin {
                backing,
                offset: source_offset,
                length: source_length,
                data_offset,
                mode,
            }),
            pages,
            backend,
            account,
            _charge: charge,
            executable,
            instructions_published: AtomicBool::new(false),
        };
        // Initial loading is batched; subsequent pressure eviction is lazy.
        empty.populate_pages(0, count)
    }

    pub(crate) fn is_file(&self) -> bool {
        self.file.is_some()
    }

    pub(crate) fn retire_file_source(&self) {
        if let Some(file) = &self.file {
            file.backing.retire();
        }
    }

    pub(crate) fn has_page(&self, index: usize) -> bool {
        self.pages.get(index).is_some_and(Option::is_some)
    }

    /// Fill contiguous holes in bounded batches. Initial executable loading must
    /// not turn one large filesystem read into a remote roundtrip for every page.
    pub(crate) fn populate_pages(
        &self,
        first: usize,
        count: usize,
    ) -> VmoResult<B, A, FallibleArc<Self>> {
        const BATCH_PAGES: usize = 64;
        let file = self.file.as_ref().ok_or(VmoError::InvalidRange)?;
        let end_index = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
        if end_index > self.pages.len() {
            return Err(VmoError::InvalidRange);
        }
        let mut next = self.clone_version()?;
        let data_end = file.data_offset + file.length;
        // Reuse resident complete pages before allocating or copying scratch.
        // This probe never performs I/O: cold contiguous holes still use the
        // bounded bulk read below. Segment edges remain private and zero-filled.
        for index in first..end_index {
            let start = index as u64 * PAGE_SIZE;
            if !next.has_page(index)
                && start >= file.data_offset
                && start + PAGE_SIZE <= data_end
                && let Some(page) = file
                    .backing
                    .cached_page(file.offset + start)
                    .map_err(|_| VmoError::BackingIo)?
            {
                next.pages[index] = Some(PrivatePage {
                    owner: super::super::own_shared_page::<B, A>(page, &self.account)?,
                    writable: false,
                });
                *next.instructions_published.get_mut() = false;
            }
        }
        if next.pages[first..end_index].iter().all(Option::is_some) {
            return FallibleArc::try_new(next).map_err(map_allocation);
        }
        let scratch_bytes = count.min(BATCH_PAGES) * PAGE_SIZE as usize;
        let _scratch_charge = self
            .account
            .try_charge(MemoryCharge {
                kernel_bytes: scratch_bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(scratch_bytes)
            .map_err(|_| VmoError::Allocation)?;
        scratch.resize(scratch_bytes, 0);
        let mut index = first;
        while index < end_index {
            if next.has_page(index) {
                index += 1;
                continue;
            }
            let batch_start = index;
            while index < end_index && index - batch_start < BATCH_PAGES && !next.has_page(index) {
                index += 1;
            }
            scratch.fill(0);
            let start = batch_start as u64 * PAGE_SIZE;
            let low = start.max(file.data_offset);
            let high = (index as u64 * PAGE_SIZE).min(data_end);
            if low < high {
                let mut done = 0;
                let length = (high - low) as usize;
                while done < length {
                    let count = file
                        .backing
                        .read_at(
                            file.offset + low + done as u64,
                            &mut scratch[(low - start) as usize + done..(high - start) as usize],
                        )
                        .map_err(|_| VmoError::BackingIo)?;
                    if count == 0 || count > length - done {
                        return Err(VmoError::BackingIo);
                    }
                    done += count;
                }
            }
            for page_index in batch_start..index {
                let page_start = page_index as u64 * PAGE_SIZE;
                let shared = if page_start >= file.data_offset && page_start + PAGE_SIZE <= data_end
                {
                    file.backing
                        .shared_page(file.offset + page_start)
                        .map_err(|_| VmoError::BackingIo)?
                } else {
                    None
                };
                if let Some(page) = shared {
                    next.pages[page_index] = Some(PrivatePage {
                        owner: super::super::own_shared_page::<B, A>(page, &self.account)?,
                        writable: false,
                    });
                    continue;
                }
                let owner = allocate_page(&self.backend, &self.account)?;
                let at = (page_index - batch_start) * PAGE_SIZE as usize;
                owner
                    .with(|owned| {
                        self.backend.write_exposed(
                            &mut owned.page,
                            0,
                            &scratch[at..at + PAGE_SIZE as usize],
                        )
                    })
                    .map_err(VmoError::Backend)?;
                next.pages[page_index] = Some(PrivatePage {
                    owner,
                    // Population policy does not confer residency authority. Only
                    // private writes or an explicit memory lock prevent reclaim.
                    writable: false,
                });
            }
            *next.instructions_published.get_mut() = false;
        }
        FallibleArc::try_new(next).map_err(map_allocation)
    }

    fn clone_version(&self) -> VmoResult<B, A, Self> {
        let charge = Self::charge(&self.account, self.pages.len())?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(self.pages.len())
            .map_err(|_| VmoError::Allocation)?;
        pages.extend(self.pages.iter().cloned());
        Ok(Self {
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
    }

    /// The old view remains alive until every machine translation is retired.
    /// Writable pages include COW output reservations and are never discarded.
    pub(crate) fn reclaim_clean(
        &self,
        first: usize,
        count: usize,
        mut locked: impl FnMut(usize) -> bool,
    ) -> VmoResult<B, A, Option<(FallibleArc<Self>, usize)>> {
        let end = first.checked_add(count).ok_or(VmoError::SizeOverflow)?;
        let candidates = self.pages.get(first..end).ok_or(VmoError::InvalidRange)?;
        if !self.is_file() || !candidates.iter().flatten().any(|p| !p.writable) {
            return Ok(None);
        }
        let mut next = self.clone_version()?;
        let mut reclaimed = 0;
        for (relative, page) in next.pages[first..end].iter_mut().enumerate() {
            if !locked(first + relative) && page.as_ref().is_some_and(|p| !p.writable) {
                *page = None;
                reclaimed += 1;
            }
        }
        if reclaimed == 0 {
            return Ok(None);
        }
        Ok(Some((
            FallibleArc::try_new(next).map_err(map_allocation)?,
            reclaimed,
        )))
    }

    pub(crate) fn publish_file_pages(
        &self,
        context: &B::InstructionPublicationContext,
    ) -> VmoResult<B, A, ()> {
        if self.executable && self.is_file() && !self.instructions_published.load(Ordering::Acquire)
        {
            self.backend
                .publish_instruction_pages(context, |visit| {
                    for page in self.pages.iter().flatten() {
                        page.owner.with(|owned| visit(&mut owned.page));
                    }
                })
                .map_err(VmoError::Backend)?;
            // Only set after publication completes. Racing preparations may
            // repeat publication but cannot use a partially published version.
            self.instructions_published.store(true, Ordering::Release);
        }
        Ok(())
    }
}
