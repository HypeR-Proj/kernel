// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! File population, finite pressure scans and process-scoped source retirement.

use super::super::vmo::FilePageSource;
use super::{
    Access, AddressSpaceError, Entry, FallibleArc, MappingIndex, MappingSet, MemoryAccount,
    PAGE_SIZE, PageBackend, PreparedMappingChange, PrivateView, SpaceResult, UserAddress,
    UserAddressSpace, UserSlice, intersection,
};

type FileReclaimPlan<'a, B, A> = Option<(PreparedMappingChange<'a, B, A>, usize)>;

impl<Backend: PageBackend, Account: MemoryAccount> UserAddressSpace<Backend, Account> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_file_view(
        &self,
        source: alloc::boxed::Box<dyn FilePageSource<Backend>>,
        size: u64,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        executable: bool,
        mode: super::PrivateMappingMode,
    ) -> SpaceResult<Backend, Account, FallibleArc<PrivateView<Backend, Account>>> {
        if self.state.with(|state| state.sealed) {
            return Err(AddressSpaceError::Stopped);
        }
        Ok(PrivateView::try_file(
            source,
            size,
            source_offset,
            source_length,
            data_offset,
            executable,
            mode,
            self.backend.clone(),
            self.account.clone(),
        )?)
    }

    /// Called after the owning process's threads are quiescent. The immutable
    /// observation pins metadata only; source guards are dropped outside MM locks.
    pub(crate) fn retire_file_sources(&self) {
        let snapshot = self.snapshot();
        for mapping in snapshot.mappings.records.iter() {
            if let Some(locks) = &mapping.locks {
                locks.retire();
            }
            if let Some(view) = mapping.object.private() {
                view.retire_file_source();
            }
        }
    }

    pub(crate) fn is_file_access(&self, address: UserAddress, access: Access) -> bool {
        self.snapshot()
            .mappings
            .records
            .containing(address)
            .is_some_and(|mapping| {
                mapping.snapshot.permissions.contains(access)
                    && mapping.object.private().is_some_and(|view| view.is_file())
            })
    }

    /// File I/O happens against an immutable observation, without the MM lock.
    /// Commit revalidates both mapping epochs, so unmap/protect cannot resurrect
    /// an obsolete fault or publish pages under changed authority.
    pub(crate) fn prepare_file_population(
        &self,
        range: UserSlice,
    ) -> SpaceResult<Backend, Account, Option<PreparedMappingChange<'_, Backend, Account>>> {
        let snapshot = self.snapshot();
        if snapshot.sealed {
            return Err(AddressSpaceError::Stopped);
        }
        let needs_population = snapshot.mappings.records.overlapping(range).any(|mapping| {
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                return false;
            };
            let Some(view) = mapping.object.private().filter(|view| view.is_file()) else {
                return false;
            };
            let offset = mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get();
            let first = (offset / PAGE_SIZE) as usize;
            let end = (offset + overlap.length()).div_ceil(PAGE_SIZE) as usize;
            (first..end).any(|index| !view.has_page(index))
        });
        if !needs_population {
            return Ok(None);
        }
        let storage_charge = self.try_mapping_set_charge()?;
        let mut replacement = snapshot.mappings.records.clone();
        let mut changed_any = false;
        for original in snapshot.mappings.records.overlapping(range) {
            let mapping = replacement
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?;
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                continue;
            };
            let Some(view) = mapping.object.private().filter(|view| view.is_file()) else {
                continue;
            };
            let offset = mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get();
            let first = (offset / PAGE_SIZE) as usize;
            let end = ((offset + overlap.length()).div_ceil(PAGE_SIZE)) as usize;
            if (first..end).all(|i| view.has_page(i)) {
                continue;
            }
            let old = view.clone();
            let next = old.populate_pages(first, end - first)?;
            replacement.replace_private_view(&old, &next, &self.account)?;
            changed_any = true;
        }
        if !changed_any {
            return Ok(None);
        }
        let replacement = FallibleArc::try_new(MappingSet {
            records: replacement,
            _storage_charge: storage_charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        let epoch = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        Ok(Some(PreparedMappingChange::new_private_backing(
            self,
            snapshot.authority_epoch,
            snapshot.mapping_epoch,
            epoch,
            replacement,
        )?))
    }

    /// A monotonically advancing address cursor visits each range at most once
    /// per pressure sweep, including views larger than one reclaim batch.
    pub(crate) fn next_file_reclaim_range(
        &self,
        after_address: u64,
        limit: usize,
    ) -> Option<UserSlice> {
        let snapshot = self.snapshot();
        let mapping = snapshot
            .mappings
            .records
            .iter_from_address(after_address)
            .find(|mapping| mapping.object.private().is_some_and(|view| view.is_file()))?;
        let range = mapping.snapshot.range;
        let start = range
            .base()
            .get()
            .max(after_address)
            .checked_add(PAGE_SIZE - 1)?
            / PAGE_SIZE
            * PAGE_SIZE;
        let bytes = range
            .end()
            .get()
            .checked_sub(start)?
            .min((limit as u64).saturating_mul(PAGE_SIZE));
        if bytes == 0 {
            return None;
        }
        UserSlice::new(UserAddress::new(start), bytes).ok()
    }

    /// Reclaims only the supplied address range. A concurrent mapping change
    /// invalidates the transaction rather than letting eviction undo a write.
    pub(crate) fn prepare_file_reclaim(
        &self,
        range: UserSlice,
    ) -> SpaceResult<Backend, Account, FileReclaimPlan<'_, Backend, Account>> {
        if !range.base().get().is_multiple_of(PAGE_SIZE)
            || !range.length().is_multiple_of(PAGE_SIZE)
        {
            return Err(AddressSpaceError::InvalidRange);
        }
        let snapshot = self.snapshot();
        let mut replacement = None;
        let mut storage_charge = None;
        let mut reclaimed = 0;
        for mapping in snapshot.mappings.records.overlapping(range) {
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                continue;
            };
            let Some(original_view) = mapping.object.private().filter(|view| view.is_file()) else {
                continue;
            };
            let view = replacement
                .as_ref()
                .and_then(|records: &MappingIndex<Backend, Account>| {
                    records
                        .by_token(mapping.snapshot.token)
                        .and_then(|record| record.object.private())
                })
                .unwrap_or(original_view);
            let first = ((mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get())
                / PAGE_SIZE) as usize;
            let Some((next, count)) = view.reclaim_clean(
                first,
                (overlap.length() / PAGE_SIZE) as usize,
                |page_index| {
                    // A private view can span several protection fragments.
                    // Never evict a page protected by any live fragment.
                    snapshot.mappings.records.iter().any(|alias| {
                        if !alias
                            .object
                            .private()
                            .is_some_and(|other| core::ptr::eq(&**other, &**original_view))
                        {
                            return false;
                        }
                        let offset = page_index as u64 * PAGE_SIZE;
                        let Some(relative) = offset.checked_sub(alias.snapshot.object_offset)
                        else {
                            return false;
                        };
                        relative < alias.snapshot.range.length()
                            && alias.locks.as_ref().is_some_and(|locks| {
                                locks.contains(alias.snapshot.range.base().get() + relative)
                            })
                    })
                },
            )?
            else {
                continue;
            };
            let old = view.clone();
            if replacement.is_none() {
                storage_charge = self.try_mapping_set_charge()?;
                replacement = Some(snapshot.mappings.records.clone());
            }
            let records = replacement
                .as_mut()
                .ok_or(AddressSpaceError::StaleMapping)?;
            records.replace_private_view(&old, &next, &self.account)?;
            reclaimed += count;
        }
        let Some(records) = replacement else {
            return Ok(None);
        };
        let replacement = FallibleArc::try_new(MappingSet {
            records,
            _storage_charge: storage_charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        let epoch = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let mut prepared = PreparedMappingChange::new_private_backing(
            self,
            snapshot.authority_epoch,
            snapshot.mapping_epoch,
            epoch,
            replacement,
        )?;
        // Reclamation only removes clean file pages; it never changes mapping
        // authority or writable private evidence. A foreign VMAR can outlive
        // process stop, so sealing must not make its clean pages unreclaimable.
        prepared.kind = super::MappingChangeKind::FileReclaim;
        Ok(Some((prepared, reclaimed)))
    }
}
