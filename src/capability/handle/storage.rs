// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Sparse fixed-page slot storage and persistent generation metadata.

use super::page::{PAGE_BYTES, Page};
use super::*;

pub(super) const SLOTS_PER_PAGE: usize = 1 << ((PAGE_BYTES / core::mem::size_of::<Slot>()).ilog2());
pub(super) const MAX_NEW_PAGES: usize = MAX_RESERVATION_SLOTS.div_ceil(SLOTS_PER_PAGE) + 1;
const DIRECTORY_CHUNKS: usize = 20;
const FIRST_CHUNK: usize = 8;
const MAX_PAGES: usize = MAX_SLOTS.div_ceil(SLOTS_PER_PAGE);

fn chunk_location(index: usize) -> (usize, usize) {
    if index < FIRST_CHUNK {
        return (0, index);
    }
    let bit = index.ilog2() as usize;
    (bit - 2, index - (1 << bit))
}
fn chunk_capacity(index: usize) -> usize {
    if index == 0 {
        FIRST_CHUNK
    } else {
        1 << (index + 2)
    }
}

pub(super) struct Directory<T> {
    chunks: [Option<Vec<T>>; DIRECTORY_CHUNKS],
}
pub(super) struct DirectoryPlan<T>(Option<(usize, Vec<T>)>);
impl<T> Directory<T> {
    pub(super) const fn new() -> Self {
        Self {
            chunks: [const { None }; DIRECTORY_CHUNKS],
        }
    }
    pub(super) fn get(&self, index: usize) -> Option<&T> {
        let (chunk, offset) = chunk_location(index);
        self.chunks.get(chunk)?.as_ref()?.get(offset)
    }
    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        let (chunk, offset) = chunk_location(index);
        self.chunks.get_mut(chunk)?.as_mut()?.get_mut(offset)
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|chunk| chunk.iter())
    }
    pub(super) fn install(&mut self, plan: DirectoryPlan<T>) {
        if let Some((index, entries)) = plan.0 {
            let Some(slot) = self.chunks.get_mut(index) else {
                super::super::invariant_violation()
            };
            if slot.is_some() {
                super::super::invariant_violation();
            }
            *slot = Some(entries);
        }
    }
    pub(super) fn growth_bytes(chunk: Option<usize>) -> Option<usize> {
        match chunk {
            Some(chunk) => chunk_capacity(chunk).checked_mul(core::mem::size_of::<T>()),
            None => Some(0),
        }
    }
}
impl<T: Default> DirectoryPlan<T> {
    pub(super) fn prepare(chunk: Option<usize>) -> Result<Self, HandleError> {
        let Some(chunk) = chunk else {
            return Ok(Self(None));
        };
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(chunk_capacity(chunk))
            .map_err(|_| HandleError::Allocation)?;
        entries.resize_with(chunk_capacity(chunk), T::default);
        Ok(Self(Some((chunk, entries))))
    }
}

#[derive(Default)]
struct Entry {
    page: Option<Page<Slot>>,
    // Maximum generation in this backing's lifetime, retained across release.
    // Reconstruction advances it (wrapping to one) and starts a new maximum.
    high_generation: u64,
    busy: usize,
    empty_previous: Option<usize>,
    empty_next: Option<usize>,
    vacant_next: Option<usize>,
}

pub(crate) struct RetiredHandleStorage {
    _directory: Directory<Entry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleTableStorageSnapshot {
    epoch: u64,
    free_slots: usize,
    pub(super) pages: [usize; MAX_NEW_PAGES],
    pub(super) count: usize,
    pub(super) directory_chunk: Option<usize>,
}
impl HandleTableStorageSnapshot {
    pub(crate) fn growth_bytes(self) -> Option<usize> {
        self.count
            .checked_mul(PAGE_BYTES)?
            .checked_add(Directory::<Entry>::growth_bytes(self.directory_chunk)?)
    }
}

#[must_use = "install or discard prepared pages"]
pub(crate) struct HandleTableStoragePlan {
    snapshot: HandleTableStorageSnapshot,
    directory: DirectoryPlan<Entry>,
    pages: [Option<(usize, Page<Slot>)>; MAX_NEW_PAGES],
}
impl HandleTableStoragePlan {
    #[inline(never)]
    pub(crate) fn try_new(snapshot: HandleTableStorageSnapshot) -> Result<Self, HandleError> {
        let directory = DirectoryPlan::prepare(snapshot.directory_chunk)?;
        let mut pages = [const { None }; MAX_NEW_PAGES];
        for (slot, &index) in pages.iter_mut().zip(&snapshot.pages[..snapshot.count]) {
            *slot = Some((index, Page::try_new(page_slots(index), |_| Slot::Empty)?));
        }
        Ok(Self {
            snapshot,
            directory,
            pages,
        })
    }
    pub(crate) const fn snapshot(&self) -> HandleTableStorageSnapshot {
        self.snapshot
    }
    #[cfg(test)]
    pub(crate) fn force_allocation_failure_for_test() -> Result<(), HandleError> {
        Page::<Slot>::try_new(usize::MAX, |_| Slot::Empty).map(drop)
    }
}

fn page_slots(index: usize) -> usize {
    SLOTS_PER_PAGE.min(MAX_SLOTS - index * SLOTS_PER_PAGE)
}
fn busy(slot: &Slot) -> bool {
    matches!(
        slot,
        Slot::Reserved { .. } | Slot::TransferReserved { .. } | Slot::Occupied { .. }
    )
}
fn generation(slot: &Slot) -> u64 {
    match slot {
        Slot::Vacant { generation, .. }
        | Slot::Reserved { generation, .. }
        | Slot::TransferReserved { generation, .. }
        | Slot::Occupied { generation, .. } => *generation,
        Slot::Empty => 0,
    }
}

pub(super) struct SlotStore {
    directory: Directory<Entry>,
    page_count: usize,
    vacant: Option<usize>,
    empty: Option<usize>,
    epoch: u64,
}
impl SlotStore {
    pub(super) const fn new() -> Self {
        Self {
            directory: Directory::new(),
            page_count: 0,
            vacant: None,
            empty: None,
            epoch: 0,
        }
    }
    pub(super) fn len(&self) -> usize {
        (self.page_count * SLOTS_PER_PAGE).min(MAX_SLOTS)
    }
    fn entry(&self, page: usize) -> &Entry {
        match self.directory.get(page) {
            Some(entry) => entry,
            None => super::super::invariant_violation(),
        }
    }
    fn entry_mut(&mut self, page: usize) -> &mut Entry {
        match self.directory.get_mut(page) {
            Some(entry) => entry,
            None => super::super::invariant_violation(),
        }
    }
    pub(super) fn get(&self, index: usize) -> Option<&Slot> {
        self.directory
            .get(index / SLOTS_PER_PAGE)?
            .page
            .as_ref()?
            .entries()
            .get(index % SLOTS_PER_PAGE)
    }
    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut Slot> {
        self.directory
            .get_mut(index / SLOTS_PER_PAGE)?
            .page
            .as_mut()?
            .entries_mut()
            .get_mut(index % SLOTS_PER_PAGE)
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.directory
            .iter()
            .filter_map(|entry| entry.page.as_ref())
            .flat_map(|page| page.entries())
    }
    pub(super) fn next_occupied(&self, start: usize) -> Option<usize> {
        let mut index = start;
        while index < self.len() {
            let page = index / SLOTS_PER_PAGE;
            if self.entry(page).page.is_none() {
                index = (page + 1) * SLOTS_PER_PAGE;
                continue;
            }
            if matches!(self.get(index), Some(Slot::Occupied { .. })) {
                return Some(index);
            }
            index += 1;
        }
        None
    }
    fn unlink_empty(&mut self, page: usize) {
        let previous = self.entry(page).empty_previous;
        let next = self.entry(page).empty_next;
        match previous {
            Some(previous) => self.entry_mut(previous).empty_next = next,
            None => self.empty = next,
        }
        if let Some(next) = next {
            self.entry_mut(next).empty_previous = previous;
        }
        let entry = self.entry_mut(page);
        entry.empty_previous = None;
        entry.empty_next = None;
    }
    fn link_empty(&mut self, page: usize) {
        let next = self.empty;
        self.entry_mut(page).empty_next = next;
        if let Some(next) = next {
            self.entry_mut(next).empty_previous = Some(page);
        }
        self.empty = Some(page);
    }
    pub(super) fn replace(&mut self, index: usize, replacement: Slot) -> Slot {
        let new_busy = busy(&replacement);
        let new_generation = generation(&replacement);
        let old = match self.get_mut(index) {
            Some(slot) => core::mem::replace(slot, replacement),
            None => super::super::invariant_violation(),
        };
        let page = index / SLOTS_PER_PAGE;
        let old_busy = busy(&old);
        self.record_generation(index, new_generation.max(generation(&old)));
        if old_busy != new_busy {
            if new_busy {
                if self.entry(page).busy == 0 {
                    self.unlink_empty(page);
                }
                self.entry_mut(page).busy += 1;
            } else {
                if self.entry(page).busy == 0 {
                    super::super::invariant_violation();
                }
                self.entry_mut(page).busy -= 1;
                if self.entry(page).busy == 0 {
                    self.link_empty(page);
                }
            }
        }
        old
    }
    pub(super) fn snapshot(
        &self,
        count: usize,
        free_slots: usize,
    ) -> Result<HandleTableStorageSnapshot, HandleError> {
        let mut result = HandleTableStorageSnapshot {
            epoch: self.epoch,
            free_slots,
            pages: [0; MAX_NEW_PAGES],
            count: 0,
            directory_chunk: None,
        };
        let mut needed = count.saturating_sub(free_slots);
        let mut vacant = self.vacant;
        let mut next = self.page_count;
        while needed > 0 {
            let page = match vacant {
                Some(page) => {
                    vacant = self.entry(page).vacant_next;
                    page
                }
                None => {
                    if next == MAX_PAGES {
                        return Err(HandleError::TableFull);
                    }
                    let page = next;
                    next += 1;
                    page
                }
            };
            result.pages[result.count] = page;
            result.count += 1;
            needed = needed.saturating_sub(page_slots(page));
            if self.directory.get(page).is_none() {
                let chunk = chunk_location(page).0;
                if result.directory_chunk.is_some_and(|found| found != chunk) {
                    super::super::invariant_violation();
                }
                result.directory_chunk = Some(chunk);
            }
        }
        Ok(result)
    }
    fn advance_epoch(&mut self) {
        self.epoch = match self.epoch.checked_add(1) {
            Some(next) => next,
            None => super::super::invariant_violation(),
        };
    }
    pub(super) fn install(
        &mut self,
        plan: &mut HandleTableStoragePlan,
    ) -> ([usize; MAX_NEW_PAGES], usize) {
        if self.epoch != plan.snapshot.epoch {
            super::super::invariant_violation();
        }
        let ids = plan.snapshot.pages;
        let count = plan.snapshot.count;
        self.directory
            .install(DirectoryPlan(plan.directory.0.take()));
        for (index, page) in plan.pages.iter_mut().filter_map(Option::take) {
            if self.entry(index).page.is_some() {
                super::super::invariant_violation();
            }
            if index == self.page_count {
                self.page_count += 1;
            } else {
                if self.vacant != Some(index) {
                    super::super::invariant_violation();
                }
                self.vacant = self.entry(index).vacant_next;
            }
            let entry = self.entry_mut(index);
            entry.vacant_next = None;
            entry.page = Some(page);
            self.link_empty(index);
        }
        if count != 0 {
            self.advance_epoch();
        }
        (ids, count)
    }
    pub(super) fn record_generation(&mut self, index: usize, generation: u64) {
        let entry = self.entry_mut(index / SLOTS_PER_PAGE);
        entry.high_generation = entry.high_generation.max(generation);
    }
    pub(super) fn fresh_generation(&mut self, page: usize) -> u64 {
        let entry = self.entry_mut(page);
        let generation = next_generation(entry.high_generation);
        entry.high_generation = generation;
        generation
    }
    pub(super) fn empty_page(&self) -> Option<usize> {
        self.empty
    }
    pub(super) fn detach_empty(&mut self, page: usize) -> Page<Slot> {
        if self.empty != Some(page) || self.entry(page).busy != 0 {
            super::super::invariant_violation();
        }
        self.unlink_empty(page);
        let backing = match self.entry_mut(page).page.take() {
            Some(page) => page,
            None => super::super::invariant_violation(),
        };
        self.entry_mut(page).vacant_next = self.vacant;
        self.vacant = Some(page);
        self.advance_epoch();
        backing
    }
    pub(super) fn page_range(&self, page: usize) -> core::ops::Range<usize> {
        page * SLOTS_PER_PAGE..page * SLOTS_PER_PAGE + page_slots(page)
    }
    pub(super) fn take_retired(&mut self) -> RetiredHandleStorage {
        self.page_count = 0;
        self.empty = None;
        self.vacant = None;
        RetiredHandleStorage {
            _directory: core::mem::replace(&mut self.directory, Directory::new()),
        }
    }
}

pub(crate) struct HandleSidecar<T> {
    codec: super::HandleCodec,
    directory: Directory<Option<Page<Option<T>>>>,
}
pub(crate) struct HandleSidecarPlan<T> {
    directory: DirectoryPlan<Option<Page<Option<T>>>>,
    pages: [Option<(usize, Page<Option<T>>)>; MAX_NEW_PAGES],
}
impl<T> HandleSidecar<T> {
    pub(super) const fn new(codec: super::HandleCodec) -> Self {
        Self {
            codec,
            directory: Directory::new(),
        }
    }
    /// Detaches storage while preserving the owning namespace for the empty index.
    pub(crate) fn take(&mut self) -> Self {
        core::mem::replace(self, Self::new(self.codec))
    }
    pub(crate) fn growth_bytes(snapshot: HandleTableStorageSnapshot) -> Option<usize> {
        snapshot.count.checked_mul(PAGE_BYTES)?.checked_add(
            Directory::<Option<Page<Option<T>>>>::growth_bytes(snapshot.directory_chunk)?,
        )
    }
    #[inline(never)]
    pub(crate) fn prepare(
        snapshot: HandleTableStorageSnapshot,
    ) -> Result<HandleSidecarPlan<T>, HandleError> {
        let directory = DirectoryPlan::prepare(snapshot.directory_chunk)?;
        let mut pages = [const { None }; MAX_NEW_PAGES];
        for (slot, &index) in pages.iter_mut().zip(&snapshot.pages[..snapshot.count]) {
            *slot = Some((index, Page::try_new(page_slots(index), |_| None)?));
        }
        Ok(HandleSidecarPlan { directory, pages })
    }
    pub(crate) fn install(&mut self, plan: &mut HandleSidecarPlan<T>) {
        self.directory
            .install(DirectoryPlan(plan.directory.0.take()));
        for (index, page) in plan.pages.iter_mut().filter_map(Option::take) {
            let target = match self.directory.get_mut(index) {
                Some(target) => target,
                None => super::super::invariant_violation(),
            };
            if target.is_some() {
                super::super::invariant_violation();
            }
            *target = Some(page);
        }
    }
    pub(crate) fn get(&self, value: HandleValue) -> Option<&T> {
        let index = self.codec.decode(value).ok()?.0;
        self.directory
            .get(index / SLOTS_PER_PAGE)?
            .as_ref()?
            .entries()
            .get(index % SLOTS_PER_PAGE)?
            .as_ref()
    }
    pub(crate) fn replace(&mut self, value: HandleValue, entry: Option<T>) -> Option<T> {
        let index = self.codec.require_decoded(value).0;
        match self
            .directory
            .get_mut(index / SLOTS_PER_PAGE)
            .and_then(Option::as_mut)
            .and_then(|page| page.entries_mut().get_mut(index % SLOTS_PER_PAGE))
        {
            Some(target) => core::mem::replace(target, entry),
            None => super::super::invariant_violation(),
        }
    }
    pub(crate) fn detach_empty(&mut self, index: usize) -> RetiredSidecarPage<T> {
        let page = match self.directory.get_mut(index).and_then(Option::take) {
            Some(page) => page,
            None => super::super::invariant_violation(),
        };
        if page.entries().iter().any(Option::is_some) {
            super::super::invariant_violation();
        }
        RetiredSidecarPage { _page: page }
    }
}
pub(crate) struct RetiredSidecarPage<T> {
    _page: Page<Option<T>>,
}
pub(crate) struct RetiredHandlePage {
    pub(super) index: usize,
    pub(super) _page: Page<Slot>,
}
impl RetiredHandlePage {
    pub(crate) fn index(&self) -> usize {
        self.index
    }
    pub(crate) fn backing_bytes(&self) -> usize {
        PAGE_BYTES * 2
    }
}
