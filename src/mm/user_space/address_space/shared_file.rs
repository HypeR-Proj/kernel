// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! File-owned mutable mappings and allocation-free revocation candidates.

use super::super::SharedSource;
use super::super::vmo::{SharedChangeGuard, SharedFileView};
use super::*;
use alloc::boxed::Box;
use hyper::hal::user::MappingUpdate;

type SharedReserves<B, A> = Option<(ReservedSharedChange<B, A>, ReservedSharedChange<B, A>)>;

pub(crate) struct ReservedSharedChange<B: PageBackend, A: MemoryAccount> {
    baseline: FallibleArc<MappingSet<B, A>>,
    replacement: FallibleArc<MappingSet<B, A>>,
    authority_epoch: u64,
    original_epoch: u64,
    base_epoch: u64,
    next_epoch: u64,
}

impl<B: PageBackend, A: MemoryAccount> ReservedSharedChange<B, A> {
    pub(crate) fn base_epoch(&self) -> u64 {
        self.base_epoch
    }
    pub(crate) fn next_epoch(&self) -> u64 {
        self.next_epoch
    }
    pub(crate) fn visit_updates(&self, visit: impl FnMut(MappingUpdate)) -> SpaceResult<B, A, ()> {
        super::updates::visit_backing_updates(&self.baseline, &self.replacement, visit)
    }
    pub(crate) fn into_prepared(
        self,
        space: &UserAddressSpace<B, A>,
        base_epoch: u64,
    ) -> SpaceResult<B, A, PreparedMappingChange<'_, B, A>> {
        if base_epoch != self.base_epoch && base_epoch != self.original_epoch {
            return Err(AddressSpaceError::StaleTransaction);
        }
        let mut prepared = PreparedMappingChange::new_private_backing(
            space,
            self.authority_epoch,
            base_epoch,
            self.next_epoch,
            self.replacement,
        )?;
        prepared.kind = MappingChangeKind::SharedWithdrawal;
        Ok(prepared)
    }
}

impl<B: PageBackend, A: MemoryAccount> PreparedMappingChange<'_, B, A> {
    pub(crate) fn prepare_shared_reserves(&self) -> SpaceResult<B, A, SharedReserves<B, A>> {
        if !self.replacement.records.iter().any(|mapping| {
            mapping
                .object
                .shared_file()
                .is_some_and(|view| view.has_resident_pages())
        }) {
            return Ok(None);
        }
        let read_only = withdrawn_set(self.address_space, &self.replacement, false)?;
        let absent = withdrawn_set(self.address_space, &read_only, true)?;
        let ro_epoch = self
            .next_mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let all_epoch = ro_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        Ok(Some((
            ReservedSharedChange {
                baseline: self.replacement.clone(),
                replacement: read_only.clone(),
                authority_epoch: self.next_authority_epoch,
                original_epoch: self.next_mapping_epoch,
                base_epoch: self.next_mapping_epoch,
                next_epoch: ro_epoch,
            },
            ReservedSharedChange {
                baseline: read_only,
                replacement: absent,
                authority_epoch: self.next_authority_epoch,
                original_epoch: self.next_mapping_epoch,
                base_epoch: ro_epoch,
                next_epoch: all_epoch,
            },
        )))
    }
}

fn withdrawn_set<B: PageBackend, A: MemoryAccount>(
    space: &UserAddressSpace<B, A>,
    baseline: &FallibleArc<MappingSet<B, A>>,
    all: bool,
) -> SpaceResult<B, A, FallibleArc<MappingSet<B, A>>> {
    let charge = space.try_mapping_set_charge()?;
    let mut records = baseline.records.clone();
    for original in baseline.records.iter() {
        let Some(old) = original.object.shared_file() else {
            continue;
        };
        let current = records
            .get(original.key())
            .and_then(|m| m.object.shared_file())
            .ok_or(AddressSpaceError::StaleMapping)?;
        if !core::ptr::eq(&**old, &**current) {
            continue;
        }
        let next = old.withdrawn(all)?;
        records.replace_shared_view(old, &next, &space.account)?;
    }
    FallibleArc::try_new(MappingSet {
        records,
        _storage_charge: charge,
    })
    .map_err(|_| AddressSpaceError::Allocation)
}

impl<B: PageBackend, A: MemoryAccount> UserAddressSpace<B, A> {
    pub(crate) fn prepare_shared_view(
        &self,
        source: B::SharedSource,
        alias: FallibleArc<Box<dyn super::super::SharedAlias>>,
        offset: u64,
        size: u64,
    ) -> SpaceResult<B, A, FallibleArc<SharedFileView<B, A>>> {
        if self.snapshot().sealed {
            return Err(AddressSpaceError::Stopped);
        }
        Ok(SharedFileView::try_new(
            source,
            alias,
            offset,
            size,
            self.backend.clone(),
            self.account.clone(),
        )?)
    }
    pub(crate) fn prepare_map_shared_view(
        &self,
        vmar: Vmar,
        range: UserSlice,
        view: FallibleArc<SharedFileView<B, A>>,
        permissions: Permissions,
        maximum: Permissions,
    ) -> SpaceResult<B, A, PreparedMappingChange<'_, B, A>> {
        self.prepare_map(
            vmar,
            range,
            MappingObject::SharedFile(view),
            0,
            permissions,
            maximum,
        )
    }
    pub(crate) fn is_shared_file_access(&self, address: UserAddress, access: Access) -> bool {
        self.snapshot()
            .mappings
            .records
            .containing(address)
            .is_some_and(|mapping| {
                mapping.object.shared_file().is_some()
                    && mapping.snapshot.permissions.contains(access)
            })
    }
    pub(crate) fn prepare_shared_population(
        &self,
        range: UserSlice,
        write: bool,
    ) -> SpaceResult<B, A, Option<PreparedMappingChange<'_, B, A>>> {
        let snapshot = self.snapshot();
        if snapshot.sealed {
            return Err(AddressSpaceError::Stopped);
        }
        let needed = snapshot.mappings.records.overlapping(range).any(|mapping| {
            let Some(view) = mapping.object.shared_file() else {
                return false;
            };
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                return false;
            };
            let start = mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get();
            (start / PAGE_SIZE..(start + overlap.length()).div_ceil(PAGE_SIZE)).any(|index| {
                !view.has_page(index as usize)
                    || (write
                        && !view
                            .writable(index * PAGE_SIZE, PAGE_SIZE as usize)
                            .unwrap_or(false))
            })
        });
        if !needed {
            return Ok(None);
        }
        let capacity = usize::try_from(range.length().div_ceil(PAGE_SIZE).saturating_add(1))
            .map_err(|_| AddressSpaceError::SizeOverflow)?;
        let charge = self.try_storage_charge::<SharedChangeGuard<B>>(
            capacity
                .checked_mul(2)
                .ok_or(AddressSpaceError::SizeOverflow)?,
        )?;
        let mut guards = Vec::new();
        guards
            .try_reserve_exact(capacity * 2)
            .map_err(|_| AddressSpaceError::Allocation)?;
        let mut records = snapshot.mappings.records.clone();
        let mut changed = false;
        for original in snapshot.mappings.records.overlapping(range) {
            let Some(overlap) = intersection(original.snapshot.range, range) else {
                continue;
            };
            let current = records
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?;
            let Some(view) = current.object.shared_file() else {
                continue;
            };
            if write && !current.snapshot.permissions.contains(Access::Write) {
                return Err(AddressSpaceError::WriteDenied);
            }
            let offset = current.snapshot.object_offset + overlap.base().get()
                - current.snapshot.range.base().get();
            let first = (offset / PAGE_SIZE) as usize;
            let end = (offset + overlap.length()).div_ceil(PAGE_SIZE) as usize;
            let old = view.clone();
            let mut next = old.clone();
            if (first..end).any(|index| !next.has_page(index)) {
                next = next.populate(first, end - first)?;
            }
            if !core::ptr::eq(&*old, &*next) {
                records.replace_shared_view(&old, &next, &self.account)?;
                changed = true;
            }
        }
        // No install or mutation admission may span source page-in/reclaim:
        // reclamation can otherwise wait for an earlier page in this very
        // transaction, and retained old roots require unwinding before refill.
        for original in snapshot.mappings.records.overlapping(range) {
            let Some(previous) = original.object.shared_file() else {
                continue;
            };
            let Some(overlap) = intersection(original.snapshot.range, range) else {
                continue;
            };
            let current = records
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?;
            let view = current
                .object
                .shared_file()
                .ok_or(AddressSpaceError::StaleMapping)?;
            let offset = current.snapshot.object_offset + overlap.base().get()
                - current.snapshot.range.base().get();
            let first = (offset / PAGE_SIZE) as usize;
            let end = (offset + overlap.length()).div_ceil(PAGE_SIZE) as usize;
            view.admit_population(previous, first, end - first, &mut guards)?;
            if write {
                let old = view.clone();
                let mut next = old.clone();
                for index in first..end {
                    if !next.writable(index as u64 * PAGE_SIZE, PAGE_SIZE as usize)? {
                        next = next.grant_write(index, &mut guards)?;
                    }
                }
                if !core::ptr::eq(&*old, &*next) {
                    records.replace_shared_view(&old, &next, &self.account)?;
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(None);
        }
        let replacement = FallibleArc::try_new(MappingSet {
            records,
            _storage_charge: self.try_mapping_set_charge()?,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        let next = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let mut prepared = PreparedMappingChange::new_private_backing(
            self,
            snapshot.authority_epoch,
            snapshot.mapping_epoch,
            next,
            replacement,
        )?;
        prepared.shared_guards = guards;
        prepared.shared_guard_charge = charge;
        Ok(Some(prepared))
    }
    pub(crate) fn sync_shared_range(
        &self,
        vmar: Vmar,
        range: UserSlice,
        scope: u32,
    ) -> SpaceResult<B, A, ()> {
        if range.length() == 0
            || !range.base().get().is_multiple_of(PAGE_SIZE)
            || !range.length().is_multiple_of(PAGE_SIZE)
            || scope > 1
        {
            return Err(AddressSpaceError::InvalidRange);
        }
        let snapshot = self.snapshot();
        validate_owned_mapped_range(self.id, self.root, &snapshot, vmar, range)?;
        let mut cursor = range.base();
        let mut source = None;
        let mut next_offset = None;
        for mapping in snapshot.mappings.records.overlapping(range) {
            let overlap = intersection(mapping.snapshot.range, range)
                .ok_or(AddressSpaceError::InvalidRange)?;
            if overlap.base() != cursor {
                return Err(AddressSpaceError::NotMapped);
            }
            if !mapping.maximum_permissions.contains(Access::Write) {
                return Err(AddressSpaceError::InvalidPermissions);
            }
            let view = mapping
                .object
                .shared_file()
                .ok_or(AddressSpaceError::InvalidRange)?;
            let offset =
                view.source_offset() + mapping.snapshot.object_offset + overlap.base().get()
                    - mapping.snapshot.range.base().get();
            if source
                .as_ref()
                .is_some_and(|first: &B::SharedSource| first.identity() != view.identity())
                || next_offset.is_some_and(|next| next != offset)
            {
                return Err(AddressSpaceError::InvalidRange);
            }
            if source.is_none() {
                source = Some(view.retain_sync()?);
            }
            next_offset = Some(offset + overlap.length());
            cursor = overlap.end();
        }
        if cursor != range.end() {
            return Err(AddressSpaceError::NotMapped);
        }
        source
            .ok_or(AddressSpaceError::NotMapped)?
            .sync(scope)
            .map_err(super::super::vmo::shared_error::<B, A>)?;
        Ok(())
    }
}
