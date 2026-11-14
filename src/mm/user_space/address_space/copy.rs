// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Resident user-copy plans and transactional output reservations.
//!
//! Plans own backing and temporary charges; write reservations pin their exact
//! byte range until the machine-level owner explicitly releases them.

use super::super::contract::{Access, MemoryAccount, PageBackend, UserSlice};
use super::super::inline_buffer::InlineBuffer;
use super::super::vmo::{MappingObject, VmoError, WritableMappingLease, WritableVmo};
use super::super::{SharedAdmission, SharedPage};
use super::AddressSpaceLock;
use super::{
    AddressSpaceError, AddressSpaceId, AddressSpaceSnapshot, SpaceResult, UserAddressSpace,
    UserWriteRange, address_space_invariant_violation, intersection,
};
use alloc::vec::Vec;
use hyper::mm::{FallibleArc, PAGE_SIZE};

impl<Backend: PageBackend, Account: MemoryAccount> UserAddressSpace<Backend, Account> {
    pub(crate) fn copy_from_user(
        &self,
        source: UserSlice,
        destination: &mut [u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        let destination_length =
            u64::try_from(destination.len()).map_err(|_| AddressSpaceError::SizeOverflow)?;
        if source.length() != destination_length {
            return Err(AddressSpaceError::SizeMismatch);
        }
        if source.length() == 0 {
            return Ok(());
        }
        let mut plan = self.prepare_copy_segments(source, Access::Read)?;
        self.prepare_shared_copy(&mut plan, Access::Read)?;
        for segment in plan.segments.iter_mut() {
            if let CopyBacking::Mapping(object) = &segment.object
                && object.shared_file().is_some()
            {
                segment.object = CopyBacking::Read(object.pin_read_range(
                    segment.object_offset,
                    usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?,
                    &self.account,
                )?);
            }
        }
        PreparedUserRead {
            plan,
            length: source.length(),
        }
        .copy_to_slice(0, destination)
    }

    /// Resolves and retains every source page before a caller acquires I/O
    /// serialization locks. A successful plan never faults on later copies.
    pub(crate) fn prepare_user_read(
        &self,
        source: UserSlice,
    ) -> SpaceResult<Backend, Account, PreparedUserRead<Backend, Account>> {
        let mut plan = self.prepare_copy_segments(source, Access::Read)?;
        self.prepare_shared_copy(&mut plan, Access::Read)?;
        for segment in plan.segments.iter_mut() {
            let CopyBacking::Mapping(object) = &segment.object else {
                return Err(AddressSpaceError::InvalidRange);
            };
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let pages = object.pin_read_range(segment.object_offset, length, &self.account)?;
            segment.object = CopyBacking::Read(pages);
            // Physical owners suffice for a read. Retaining a writable VMO
            // lease here would unnecessarily keep its entire sparse page set.
            segment._write_lease = None;
        }
        Ok(PreparedUserRead {
            plan,
            length: source.length(),
        })
    }

    pub(crate) fn copy_to_user(
        &self,
        destination: UserSlice,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        let source_length =
            u64::try_from(source.len()).map_err(|_| AddressSpaceError::SizeOverflow)?;
        if destination.length() != source_length {
            return Err(AddressSpaceError::SizeMismatch);
        }
        if destination.length() == 0 {
            return Ok(());
        }
        let mut plan = self.prepare_copy_segments(destination, Access::Write)?;
        self.prepare_shared_copy(&mut plan, Access::Write)?;
        let _admission = plan.admit_shared_write()?;
        plan.copy_from(source)
    }

    /// Pins the current writable mapping set for a later transactional copy.
    ///
    /// While the returned plan is active, authority changes intersecting its
    /// exact byte range return `Busy`; unrelated mappings may still change.
    /// This closes the interval between writing future capability values and
    /// publishing those values in a Process handle table: another Thread
    /// cannot redirect the output range to unrelated backing in that window.
    pub(in super::super) fn prepare_user_write(
        &self,
        destination: UserSlice,
    ) -> Result<
        PreparedUserWrite<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        let snapshot = self.snapshot();
        let mut plan = self.prepare_copy_from_snapshot(destination, Access::Write, &snapshot)?;
        self.prepare_shared_copy(&mut plan, Access::Write)?;
        plan.pin_private_ranges()?;
        let (reservation_id, count) = self
            .state
            .with(|state| (state.next_user_write_id, state.user_writes.len()));
        let next_id = reservation_id
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let capacity = count
            .checked_add(1)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let mut charge = self.try_storage_charge::<UserWriteRange>(
            InlineBuffer::<UserWriteRange>::allocated_capacity(capacity),
        )?;
        let mut records =
            InlineBuffer::try_with_capacity(capacity).map_err(|_| AddressSpaceError::Allocation)?;
        let retired = self.state.with(|state| {
            if state.sealed {
                return Err(AddressSpaceError::Stopped);
            }
            if state.mapping_epoch != snapshot.mapping_epoch
                || state.next_user_write_id != reservation_id
            {
                return Err(AddressSpaceError::StaleTransaction);
            }
            // Other admissions change next_user_write_id. Releases only shrink
            // the set, so this preallocated capacity covers all current records
            // plus our new one. Only Copy metadata moves with IRQs masked.
            for record in state.user_writes.iter().copied() {
                records.push(record);
            }
            records.push(UserWriteRange {
                id: reservation_id,
                range: destination,
            });
            let old_records =
                core::mem::replace(&mut state.user_writes, core::mem::take(&mut records));
            let old_charge = core::mem::replace(&mut state.user_write_charge, charge.take());
            state.next_user_write_id = next_id;
            Ok((old_records, old_charge))
        })?;
        // On stale admission, uninstalled records/charge also remain outside
        // the state lock. No allocation or destructor runs in the metadata cut.
        drop(retired);
        Ok(PreparedUserWrite {
            address_space: self.id,
            reservation_id,
            plan,
            completed: false,
        })
    }

    pub(in super::super) fn write_user_reservation(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_from(source)
    }

    pub(in super::super) fn write_user_reservation_prefix(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_prefix_from(source)
    }

    pub(in super::super) fn write_user_reservation_parts<'a>(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        parts: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> SpaceResult<Backend, Account, ()> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_parts_from(parts)
    }

    pub(in super::super) fn release_user_write(
        &self,
        mut reservation: PreparedUserWrite<Backend, Account>,
    ) {
        if reservation.address_space != self.id || reservation.completed {
            address_space_invariant_violation();
        }
        let retired = self.state.with(|state| {
            let Some(index) = state
                .user_writes
                .iter()
                .position(|record| record.id == reservation.reservation_id)
            else {
                address_space_invariant_violation();
            };
            state.user_writes.swap_remove(index);
            if state.user_writes.is_empty() {
                Some((
                    core::mem::take(&mut state.user_writes),
                    state.user_write_charge.take(),
                ))
            } else {
                None
            }
        });
        // The last release returns all registry storage without allocation.
        drop(retired);
        reservation.completed = true;
    }

    #[cfg(test)]
    pub(crate) fn prepare_user_write_for_test(
        &self,
        destination: UserSlice,
    ) -> Result<
        PreparedUserWrite<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        self.prepare_user_write(destination)
    }

    #[cfg(test)]
    pub(crate) fn write_user_reservation_for_test(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        self.write_user_reservation(reservation, source)
    }

    #[cfg(test)]
    pub(crate) fn write_user_reservation_prefix_for_test(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        self.write_user_reservation_prefix(reservation, source)
    }

    #[cfg(test)]
    pub(crate) fn write_user_reservation_parts_for_test<'a>(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        parts: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> SpaceResult<Backend, Account, ()> {
        self.write_user_reservation_parts(reservation, parts)
    }

    #[cfg(test)]
    pub(crate) fn release_user_write_for_test(
        &self,
        reservation: PreparedUserWrite<Backend, Account>,
    ) {
        self.release_user_write(reservation);
    }

    fn prepare_copy_segments(
        &self,
        range: UserSlice,
        access: Access,
    ) -> Result<CopyPlan<Backend, Account>, AddressSpaceError<Backend::Error, Account::Error>> {
        let snapshot = self.snapshot();
        self.prepare_copy_from_snapshot(range, access, &snapshot)
    }

    // Prepare only mapping-backed segments. The operation entry points finish
    // shared sources/pins on their retained plan after all traversal frames
    // return, before exposing the plan to any copy operation.
    #[inline(never)]
    fn prepare_copy_from_snapshot(
        &self,
        range: UserSlice,
        access: Access,
        snapshot: &AddressSpaceSnapshot<Backend, Account>,
    ) -> Result<CopyPlan<Backend, Account>, AddressSpaceError<Backend::Error, Account::Error>> {
        if snapshot.sealed {
            return Err(AddressSpaceError::Stopped);
        }
        if !self.root.range.contains(range) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let count = snapshot.mappings.records.overlapping(range).count();
        let temporary_charge =
            self.try_storage_charge::<CopySegment<Backend, Account>>(InlineBuffer::<
                CopySegment<Backend, Account>,
            >::allocated_capacity(
                count
            ))?;
        let mut segments =
            InlineBuffer::try_with_capacity(count).map_err(|_| AddressSpaceError::Allocation)?;
        let mut cursor = range.base();
        for mapping in snapshot.mappings.records.overlapping(range) {
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                continue;
            };
            if overlap.base() != cursor {
                return Err(AddressSpaceError::NotMapped);
            }
            if !mapping.snapshot.permissions.contains(access) {
                return Err(match access {
                    Access::Write => AddressSpaceError::WriteDenied,
                    Access::Read | Access::Execute => AddressSpaceError::ReadDenied,
                });
            }
            let delta = overlap
                .base()
                .get()
                .checked_sub(mapping.snapshot.range.base().get())
                .ok_or(AddressSpaceError::SizeOverflow)?;
            let object_offset = mapping
                .snapshot
                .object_offset
                .checked_add(delta)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            if access == Access::Write
                && mapping.object.shared_file().is_none()
                && !mapping.object.writable_backing(
                    object_offset,
                    usize::try_from(overlap.length())
                        .map_err(|_| AddressSpaceError::SizeOverflow)?,
                )?
            {
                return Err(AddressSpaceError::CowRequired);
            }
            if let Some(view) = mapping.object.shared_file() {
                let end = (object_offset + overlap.length()).div_ceil(PAGE_SIZE);
                for index in object_offset / PAGE_SIZE..end {
                    view.page(index as usize)?;
                }
            }
            segments.push(CopySegment {
                object: CopyBacking::Mapping(mapping.object.clone()),
                _write_lease: mapping.write_lease.clone(),
                object_offset: mapping
                    .snapshot
                    .object_offset
                    .checked_add(delta)
                    .ok_or(AddressSpaceError::SizeOverflow)?,
                length: overlap.length(),
            });
            cursor = overlap.end();
        }
        if cursor != range.end() {
            return Err(AddressSpaceError::NotMapped);
        }
        Ok(CopyPlan {
            segments,
            shared: None,
            _shared_sources: Vec::new(),
            _shared_source_charge: None,
            _temporary_charge: temporary_charge,
        })
    }

    // Runs after the mapping-traversal frame has returned. Borrow the caller's
    // retained plan so pin/source admission never nests beneath traversal
    // scratch or carries a second owned copy of the backing records.
    // Segments retain their mapping origins after snapshot release; retain_sync
    // separately revalidates explicit source retirement before admitting I/O.
    #[inline(never)]
    fn prepare_shared_copy(
        &self,
        plan: &mut CopyPlan<Backend, Account>,
        access: Access,
    ) -> SpaceResult<Backend, Account, ()> {
        let segments = &plan.segments;
        // A physical pin keeps bytes alive but does not retain namespace or
        // mount authority. Keep a separate operation lease even after read
        // segments are converted to bare physical owners. Last unmap can then
        // retire its mapping lease without making an admitted copy invisible
        // to unlink, rename or unmount.
        let shared_count=segments.iter().filter(|segment|matches!(&segment.object,CopyBacking::Mapping(object) if object.shared_file().is_some())).count();
        plan._shared_source_charge =
            self.try_storage_charge::<Backend::SharedSource>(shared_count)?;
        plan._shared_sources
            .try_reserve_exact(shared_count)
            .map_err(|_| AddressSpaceError::Allocation)?;
        for segment in segments.iter() {
            if let CopyBacking::Mapping(object) = &segment.object
                && let Some(view) = object.shared_file()
            {
                plan._shared_sources.push(view.retain_sync()?);
            }
        }
        let has_shared = access == Access::Write && segments.iter().any(|segment| matches!(&segment.object, CopyBacking::Mapping(object) if object.shared_file().is_some()));
        plan.shared = if has_shared {
            let charge = Some(
                self.account
                    .try_charge(super::super::MemoryCharge {
                        kernel_bytes:
                            FallibleArc::<SharedOutputState<Backend, Account>>::allocation_size()
                                as u64,
                        ..Default::default()
                    })
                    .map_err(AddressSpaceError::Account)?,
            );
            // Reserve the final shared owner before assembling its pins and
            // accounting state. Allocation failure must not retain a second
            // complete value on the user-copy admission stack.
            let slot = hyper::mm::UniqueFallibleArc::try_new_uninit()
                .map_err(|_| AddressSpaceError::Allocation)?;
            let count = segments.iter().filter(|segment|matches!(&segment.object,CopyBacking::Mapping(object) if object.shared_file().is_some())).try_fold(0usize,|total,segment| total.checked_add(((segment.object_offset%PAGE_SIZE+segment.length).div_ceil(PAGE_SIZE)) as usize).ok_or(AddressSpaceError::SizeOverflow))?;
            let pin_charge = self
                .account
                .try_charge(super::super::MemoryCharge {
                    kernel_bytes: count
                        .checked_mul(
                            core::mem::size_of::<<Backend::SharedPage as SharedPage>::Pin>(),
                        )
                        .ok_or(AddressSpaceError::SizeOverflow)?
                        as u64,
                    ..Default::default()
                })
                .map_err(AddressSpaceError::Account)?;
            let mut pins = Vec::new();
            pins.try_reserve_exact(count)
                .map_err(|_| AddressSpaceError::Allocation)?;
            for segment in segments.iter() {
                let CopyBacking::Mapping(object) = &segment.object else {
                    continue;
                };
                let Some(view) = object.shared_file() else {
                    continue;
                };
                for index in segment.object_offset / PAGE_SIZE
                    ..(segment.object_offset + segment.length).div_ceil(PAGE_SIZE)
                {
                    pins.push(
                        view.page(index as usize)?
                            .try_pin()
                            .map_err(super::super::vmo::shared_error::<Backend, Account>)?,
                    );
                }
            }
            Some(
                slot.write(SharedOutputState {
                    current: AddressSpaceLock::new(None),
                    account: self.account.clone(),
                    _charge: charge,
                    _pins: pins,
                    _pin_charge: pin_charge,
                })
                .into_shared(),
            )
        } else {
            None
        };
        Ok(())
    }
}

enum CopyBacking<B: PageBackend, A: MemoryAccount> {
    Mapping(MappingObject<B, A>),
    Private(super::super::vmo::PinnedPrivateRange<B, A>),
    Read(super::super::vmo::PinnedReadRange<B, A>),
}

impl<B: PageBackend, A: MemoryAccount> CopyBacking<B, A> {
    fn read_exposed(
        &self,
        offset: u64,
        bytes: &mut [u8],
    ) -> Result<(), VmoError<B::Error, A::Error>> {
        match self {
            Self::Mapping(object) => object.read_exposed(offset, bytes),
            Self::Private(pages) => pages.read_exposed(offset, bytes),
            Self::Read(pages) => pages.read_exposed(offset, bytes),
        }
    }
    fn write_exposed(&self, offset: u64, bytes: &[u8]) -> Result<(), VmoError<B::Error, A::Error>> {
        match self {
            Self::Mapping(object) => object.write_exposed(offset, bytes),
            Self::Private(pages) => pages.write_exposed(offset, bytes),
            Self::Read(_) => Err(VmoError::InvalidRange),
        }
    }
}

struct CopySegment<Backend: PageBackend, Account: MemoryAccount> {
    object: CopyBacking<Backend, Account>,
    _write_lease: Option<WritableMappingLease<Backend, Account>>,
    object_offset: u64,
    length: u64,
}

struct CopyPlan<Backend: PageBackend, Account: MemoryAccount> {
    segments: InlineBuffer<CopySegment<Backend, Account>>,
    _temporary_charge: Option<Account::Charge>,
    shared: Option<FallibleArc<SharedOutputState<Backend, Account>>>,
    _shared_sources: Vec<Backend::SharedSource>,
    _shared_source_charge: Option<Account::Charge>,
}

impl<Backend: PageBackend, Account: MemoryAccount> CopyPlan<Backend, Account> {
    fn pin_private_ranges(&mut self) -> SpaceResult<Backend, Account, ()> {
        for segment in self.segments.iter_mut() {
            let CopyBacking::Mapping(object) = &segment.object else {
                continue;
            };
            let Some(view) = object.private() else {
                continue;
            };
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let pages = view.pin_write_range(segment.object_offset, length)?;
            segment.object = CopyBacking::Private(pages);
        }
        Ok(())
    }

    fn copy_from(
        &self,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        let expected = self.segments.iter().try_fold(0usize, |total, segment| {
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            total
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)
        })?;
        if expected != source.len() {
            return Err(AddressSpaceError::SizeMismatch);
        }
        let mut copied = 0usize;
        for segment in self.segments.iter() {
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let end = copied
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            let bytes = source
                .get(copied..end)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            self.write_segment(segment, segment.object_offset, bytes)?;
            copied = end;
        }
        Ok(())
    }

    fn copy_parts_from<'a>(
        &self,
        mut parts: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> SpaceResult<Backend, Account, ()> {
        let capacity = self.segments.iter().try_fold(0u64, |sum, segment| {
            sum.checked_add(segment.length)
                .ok_or(AddressSpaceError::SizeOverflow)
        })?;
        let length = parts.clone().try_fold(0u64, |sum, bytes| {
            sum.checked_add(bytes.len() as u64)
                .ok_or(AddressSpaceError::SizeOverflow)
        })?;
        if length != capacity {
            return Err(AddressSpaceError::SizeMismatch);
        }
        let mut source = &[][..];
        for segment in self.segments.iter() {
            let mut copied = 0;
            while copied < segment.length {
                while source.is_empty() {
                    source = parts.next().ok_or(AddressSpaceError::SizeMismatch)?;
                }
                let count = source.len().min((segment.length - copied) as usize);
                self.write_segment(segment, segment.object_offset + copied, &source[..count])?;
                source = &source[count..];
                copied += count as u64;
            }
        }
        Ok(())
    }

    /// Copies only the initialized prefix while retaining the complete pinned
    /// mapping plan. Capability receive uses this to leave unused output
    /// capacity byte-for-byte unchanged.
    fn copy_prefix_from(
        &self,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        let capacity = self.segments.iter().try_fold(0usize, |total, segment| {
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            total
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)
        })?;
        if source.len() > capacity {
            return Err(AddressSpaceError::SizeMismatch);
        }

        let mut copied = 0usize;
        for segment in self.segments.iter() {
            if copied == source.len() {
                break;
            }
            let segment_length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let remaining = source.len() - copied;
            let length = segment_length.min(remaining);
            let end = copied
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            let bytes = source
                .get(copied..end)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            self.write_segment(segment, segment.object_offset, bytes)?;
            copied = end;
        }
        if copied != source.len() {
            return Err(AddressSpaceError::SizeMismatch);
        }
        Ok(())
    }
}

/// Resident source owners independent of subsequent user mapping changes.
pub(crate) struct PreparedUserRead<Backend: PageBackend, Account: MemoryAccount> {
    plan: CopyPlan<Backend, Account>,
    length: u64,
}

impl<B: PageBackend, A: MemoryAccount> PreparedUserRead<B, A> {
    pub(crate) fn len(&self) -> u64 {
        self.length
    }

    pub(crate) fn copy_to_slice(
        &self,
        source_offset: u64,
        output: &mut [u8],
    ) -> SpaceResult<B, A, ()> {
        let end = source_offset
            .checked_add(output.len() as u64)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        if end > self.length {
            return Err(AddressSpaceError::InvalidRange);
        }
        let mut start = 0;
        let mut copied = 0;
        for segment in self.plan.segments.iter() {
            let next = start + segment.length;
            let low = source_offset.max(start);
            let high = end.min(next);
            if low < high {
                let length =
                    usize::try_from(high - low).map_err(|_| AddressSpaceError::SizeOverflow)?;
                segment.object.read_exposed(
                    segment.object_offset + low - start,
                    &mut output[copied..copied + length],
                )?;
                copied += length;
            }
            start = next;
        }
        if copied != output.len() {
            return Err(AddressSpaceError::SizeMismatch);
        }
        Ok(())
    }

    /// Copies a subrange through resident page owners. The user can still
    /// mutate its bytes; this is an ordinary read, not an atomic snapshot.
    pub(crate) fn copy_to_vmo(
        &self,
        destination: &WritableVmo<B, A>,
        source_offset: u64,
        destination_offset: u64,
        length: usize,
    ) -> SpaceResult<B, A, ()> {
        let _access = destination.begin_kernel_write()?;
        self.copy_to_resident_vmo(destination, source_offset, destination_offset, length)
    }

    /// Copies into the externally writable backing selected by this lease.
    /// The lease excludes snapshots and hardware ownership throughout the
    /// copy. A protocol must establish when peer readers may consume the bytes;
    /// this operation neither creates Rust aliases nor promises atomic contents.
    pub(crate) fn copy_to_exposed_vmo(
        &self,
        destination: &WritableMappingLease<B, A>,
        source_offset: u64,
        destination_offset: u64,
        length: usize,
    ) -> SpaceResult<B, A, ()> {
        self.copy_to_resident_vmo(
            &destination.exposed_copy_target(),
            source_offset,
            destination_offset,
            length,
        )
    }

    fn copy_to_resident_vmo(
        &self,
        destination: &WritableVmo<B, A>,
        source_offset: u64,
        destination_offset: u64,
        length: usize,
    ) -> SpaceResult<B, A, ()> {
        let end = source_offset
            .checked_add(length as u64)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let destination_end = destination_offset
            .checked_add(length as u64)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        if end > self.length || destination_end > destination.size() {
            return Err(AddressSpaceError::InvalidRange);
        }
        let mut start = 0;
        let mut copied = 0;
        for segment in self.plan.segments.iter() {
            let next = start + segment.length;
            let low = source_offset.max(start);
            let high = end.min(next);
            if low < high {
                let CopyBacking::Read(pages) = &segment.object else {
                    return Err(AddressSpaceError::InvalidRange);
                };
                let count =
                    usize::try_from(high - low).map_err(|_| AddressSpaceError::SizeOverflow)?;
                pages.copy_into(
                    low - start,
                    destination,
                    destination_offset + copied as u64,
                    count,
                )?;
                copied += count;
            }
            start = next;
        }
        if copied != length {
            return Err(AddressSpaceError::SizeMismatch);
        }
        Ok(())
    }
}

/// Mapping-stable copy plan owned by the machine-level reservation.
#[must_use = "release the mapping reservation on every exit path"]
pub(crate) struct PreparedUserWrite<Backend: PageBackend, Account: MemoryAccount> {
    address_space: AddressSpaceId,
    reservation_id: u64,
    plan: CopyPlan<Backend, Account>,
    completed: bool,
}

impl<B: PageBackend, A: MemoryAccount> PreparedUserWrite<B, A> {
    pub(crate) fn admit_shared_write(&self) -> SpaceResult<B, A, SharedOutputAdmission<B, A>> {
        self.plan.admit_shared_write()
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop for PreparedUserWrite<Backend, Account> {
    fn drop(&mut self) {
        if !self.completed {
            address_space_invariant_violation();
        }
    }
}

struct SharedMutationSet<B: PageBackend, A: MemoryAccount> {
    entries: Vec<(u64, <B::SharedPage as SharedPage>::Mutation)>,
    _charge: A::Charge,
}
struct SharedOutputState<B: PageBackend, A: MemoryAccount> {
    current: AddressSpaceLock<Option<FallibleArc<SharedMutationSet<B, A>>>>,
    account: A,
    _charge: Option<A::Charge>,
    _pins: Vec<<B::SharedPage as SharedPage>::Pin>,
    _pin_charge: A::Charge,
}
/// Owns only short mutation admissions; never retain across peer waits.
pub(crate) struct SharedOutputAdmission<B: PageBackend, A: MemoryAccount> {
    state: Option<FallibleArc<SharedOutputState<B, A>>>,
}
impl<B: PageBackend, A: MemoryAccount> Drop for SharedOutputAdmission<B, A> {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            let retired = state.current.with(Option::take);
            drop(retired);
        }
    }
}
impl<B: PageBackend, A: MemoryAccount> CopyPlan<B, A> {
    fn admit_shared_write(&self) -> SpaceResult<B, A, SharedOutputAdmission<B, A>> {
        let Some(state) = &self.shared else {
            return Ok(SharedOutputAdmission { state: None });
        };
        if state.current.with(|current| current.is_some()) {
            return Err(AddressSpaceError::Busy);
        }
        let count=self.segments.iter().try_fold(0usize,|count,segment| {
            if matches!(&segment.object,CopyBacking::Mapping(object) if object.shared_file().is_some()) {
                count.checked_add(((segment.object_offset%PAGE_SIZE+segment.length).div_ceil(PAGE_SIZE)) as usize).ok_or(AddressSpaceError::SizeOverflow)
            } else {Ok(count)}
        })?;
        let bytes = count
            .checked_mul(core::mem::size_of::<(
                u64,
                <B::SharedPage as SharedPage>::Mutation,
            )>())
            .and_then(|bytes| {
                bytes.checked_add(FallibleArc::<SharedMutationSet<B, A>>::allocation_size())
            })
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let charge = state
            .account
            .try_charge(super::super::MemoryCharge {
                kernel_bytes: bytes as u64,
                ..Default::default()
            })
            .map_err(AddressSpaceError::Account)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| AddressSpaceError::Allocation)?;
        for segment in self.segments.iter() {
            let CopyBacking::Mapping(object) = &segment.object else {
                continue;
            };
            let Some(view) = object.shared_file() else {
                continue;
            };
            for index in segment.object_offset / PAGE_SIZE
                ..(segment.object_offset + segment.length).div_ceil(PAGE_SIZE)
            {
                let page = view.page(index as usize)?;
                if entries
                    .iter()
                    .any(|(identity, _)| *identity == page.identity())
                {
                    continue;
                }
                let guard = page
                    .try_begin_write()
                    .map_err(super::super::vmo::shared_error::<B, A>)?;
                entries.push((page.identity(), guard));
            }
        }
        let owned = FallibleArc::try_new(SharedMutationSet {
            entries,
            _charge: charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        state.current.with(|current| {
            if current.is_some() {
                return Err(AddressSpaceError::Busy);
            }
            *current = Some(owned.clone());
            Ok(())
        })?;
        Ok(SharedOutputAdmission {
            state: Some(state.clone()),
        })
    }
    fn write_segment(
        &self,
        segment: &CopySegment<B, A>,
        offset: u64,
        bytes: &[u8],
    ) -> SpaceResult<B, A, ()> {
        let CopyBacking::Mapping(object) = &segment.object else {
            return Ok(segment.object.write_exposed(offset, bytes)?);
        };
        let Some(view) = object.shared_file() else {
            return Ok(segment.object.write_exposed(offset, bytes)?);
        };
        let state = self.shared.as_ref().ok_or(AddressSpaceError::Busy)?;
        let admitted = state
            .current
            .with(|current| current.clone())
            .ok_or(AddressSpaceError::Busy)?;
        let mut copied = 0;
        while copied < bytes.len() {
            let absolute = offset + copied as u64;
            let page = view.page((absolute / PAGE_SIZE) as usize)?;
            let guard = &admitted
                .entries
                .iter()
                .find(|(id, _)| *id == page.identity())
                .ok_or(AddressSpaceError::Busy)?
                .1;
            if !guard.valid() {
                return Err(AddressSpaceError::Busy);
            }
            let at = (absolute % PAGE_SIZE) as usize;
            let length = (bytes.len() - copied).min(PAGE_SIZE as usize - at);
            page.write_exposed(guard, at, &bytes[copied..copied + length])
                .map_err(super::super::vmo::shared_error::<B, A>)?;
            copied += length;
        }
        Ok(())
    }
}
