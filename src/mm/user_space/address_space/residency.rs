// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit, transactional residency grants for existing virtual mappings.
//!
//! The immutable mapping set owns one charge per locked virtual page. Mapping
//! generations serialize locking with unmap, COW and pressure eviction. Retired
//! versions retain their grants until machine translations have been retired.

use super::super::vmo::UserMemoryLock;
use super::*;

struct LockedPage<A: MemoryAccount> {
    charge: UserMemoryLock<Option<A::Charge>>,
}

impl<A: MemoryAccount> LockedPage<A> {
    fn active(&self) -> bool {
        self.charge.with(|charge| charge.is_some())
    }
    fn retire(&self) {
        drop(self.charge.with(Option::take));
    }
}

pub(super) struct LockedPages<A: MemoryAccount> {
    pages: Vec<(u64, FallibleArc<LockedPage<A>>)>,
    account: A,
    _charge: A::Charge,
}

impl<A: MemoryAccount> LockedPages<A> {
    pub(super) fn contains(&self, address: u64) -> bool {
        self.pages
            .binary_search_by_key(&address, |entry| entry.0)
            .ok()
            .is_some_and(|index| self.pages[index].1.active())
    }

    pub(super) fn retire(&self) {
        for (_, page) in &self.pages {
            page.retire();
        }
    }

    fn build<B: PageBackend>(account: &A, capacity: usize) -> SpaceResult<B, A, Self> {
        let bytes = capacity
            .checked_mul(core::mem::size_of::<(u64, FallibleArc<LockedPage<A>>)>())
            .and_then(|bytes| bytes.checked_add(FallibleArc::<Self>::allocation_size()))
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(capacity)
            .map_err(|_| AddressSpaceError::Allocation)?;
        Ok(Self {
            pages,
            account: account.clone(),
            _charge: charge,
        })
    }

    pub(super) fn retain<B: PageBackend>(
        &self,
        range: UserSlice,
    ) -> SpaceResult<B, A, Option<FallibleArc<Self>>> {
        let count = self
            .pages
            .iter()
            .filter(|(address, _)| *address >= range.base().get() && *address < range.end().get())
            .count();
        if count == 0 {
            return Ok(None);
        }
        let mut next = Self::build::<B>(&self.account, count)?;
        next.pages.extend(
            self.pages
                .iter()
                .filter(|(address, _)| {
                    *address >= range.base().get() && *address < range.end().get()
                })
                .cloned(),
        );
        Ok(Some(
            FallibleArc::try_new(next).map_err(|_| AddressSpaceError::Allocation)?,
        ))
    }

    fn update<B: PageBackend>(
        account: &A,
        original: Option<&Self>,
        affected: UserSlice,
        lock: bool,
    ) -> SpaceResult<B, A, Option<FallibleArc<Self>>> {
        let previous = original.map_or(&[][..], |set| set.pages.as_slice());
        let first = previous.partition_point(|(address, _)| *address < affected.base().get());
        let end = previous.partition_point(|(address, _)| *address < affected.end().get());
        let affected_count = if lock {
            usize::try_from(affected.length() / PAGE_SIZE)
                .map_err(|_| AddressSpaceError::SizeOverflow)?
        } else {
            0
        };
        let capacity = previous.len() - (end - first);
        let capacity = capacity
            .checked_add(affected_count)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        if capacity == 0 {
            return Ok(None);
        }
        // Sparse range locks need metadata proportional to the actual grants,
        // not the containing mapping's potentially much larger address range.
        let mut next = Self::build::<B>(account, capacity)?;
        next.pages.extend(previous[..first].iter().cloned());
        let mut existing = previous[first..end].iter().peekable();
        for index in 0..affected_count {
            let address = affected.base().get() + index as u64 * PAGE_SIZE;
            if existing.peek().is_some_and(|(old, _)| *old == address) {
                let (_, page) = existing.next().ok_or(AddressSpaceError::InvalidRange)?;
                if page.active() {
                    next.pages.push((address, page.clone()));
                    continue;
                }
            }
            let charge = account
                .try_charge(MemoryCharge {
                    pinned_pages: 1,
                    kernel_bytes: FallibleArc::<LockedPage<A>>::allocation_size() as u64,
                    ..MemoryCharge::default()
                })
                .map_err(AddressSpaceError::Account)?;
            next.pages.push((
                address,
                FallibleArc::try_new(LockedPage {
                    charge: UserMemoryLock::new(Some(charge)),
                })
                .map_err(|_| AddressSpaceError::Allocation)?,
            ));
        }
        next.pages.extend(previous[end..].iter().cloned());
        Ok(Some(
            FallibleArc::try_new(next).map_err(|_| AddressSpaceError::Allocation)?,
        ))
    }
}

impl<B: PageBackend, A: MemoryAccount> UserAddressSpace<B, A> {
    /// Locks/unlocks a fixed current snapshot. None selects all mappings within
    /// this VMAR, including descendants, but creates no policy for future maps.
    /// Partial failure may warm caches; it never publishes partial lock state.
    pub(crate) fn prepare_residency(
        &self,
        vmar: Vmar,
        range: Option<UserSlice>,
        lock: bool,
    ) -> SpaceResult<B, A, PreparedMappingChange<'_, B, A>> {
        let snapshot = self.snapshot();
        let authority = validate_vmar(self.id, self.root, &snapshot, vmar)?;
        let affected = range.unwrap_or(authority);
        if let Some(range) = range {
            require_nonempty_aligned(range)?;
            if !authority.contains(range) {
                return Err(AddressSpaceError::InvalidRange);
            }
            let mut cursor = range.base();
            for mapping in snapshot.mappings.records.overlapping(range) {
                if let Some(overlap) = intersection(mapping.snapshot.range, range) {
                    if overlap.base() != cursor {
                        return Err(AddressSpaceError::NotMapped);
                    }
                    cursor = overlap.end();
                }
            }
            if cursor != range.end() {
                return Err(AddressSpaceError::NotMapped);
            }
        }
        let storage_charge = self.try_mapping_set_charge()?;
        let mut records = snapshot.mappings.records.clone();
        for original in snapshot.mappings.records.overlapping(affected) {
            let mapping = records
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?;
            let Some(overlap) = intersection(mapping.snapshot.range, affected) else {
                continue;
            };
            if lock && let Some(view) = mapping.object.private().filter(|view| view.is_file()) {
                let old = view.clone();
                let offset = mapping.snapshot.object_offset + overlap.base().get()
                    - mapping.snapshot.range.base().get();
                let first = (offset / PAGE_SIZE) as usize;
                let count = (overlap.length() / PAGE_SIZE) as usize;
                if (first..first + count).any(|page| !old.has_page(page)) {
                    let next = old.populate_pages(first, count)?;
                    records.replace_private_view(&old, &next, &self.account)?;
                }
            }
            let mut mapping = records
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?
                .clone();
            mapping.locks =
                LockedPages::update::<B>(&self.account, mapping.locks.as_deref(), overlap, lock)?;
            records = records.insert(mapping, &self.account)?;
        }
        let replacement = FallibleArc::try_new(MappingSet {
            records,
            _storage_charge: storage_charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        let epoch = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        PreparedMappingChange::new_private_backing(
            self,
            snapshot.authority_epoch,
            snapshot.mapping_epoch,
            epoch,
            replacement,
        )
    }
}
