// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concurrent VMAR authority and transactional native address-space mappings.

mod file;
mod observation;
mod records;
mod residency;
mod updates;
#[cfg(test)]
use hyper::mm::PhysicalAddress;
pub(crate) use observation::{CrashMapping, CrashMappingKind, CrashMemory, CrashReadError};

use alloc::vec::Vec;
use core::mem::{ManuallyDrop, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

use hyper::mm::{FallibleArc, ForeignMemory, PAGE_SIZE};
use hyper::sync::InterruptSpinLock;

use super::contract::{
    Access, MemoryAccount, MemoryCharge, PageBackend, Permissions, UserAddress, UserAddressWindow,
    UserSlice,
};
use super::free_ranges::FreeRanges;
use super::index::{self, Entry, Index};
use super::inline_buffer::InlineBuffer;
use records::MappingIndex;

use super::vmo::{
    ExecutableVmo, MappingObject, PrivateMappingMode, PrivateView, SnapshotVmo, VmoError,
    WritableMappingLease, WritableVmo,
};

#[cfg(not(test))]
type AddressSpaceLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[cfg(test)]
struct TestInterruptMask;

#[cfg(test)]
impl hyper::hal::interrupt::InterruptMask for TestInterruptMask {
    type State = ();

    fn save_and_disable() -> Self::State {}
    fn restore(_: Self::State) {}
    fn wait_for_lock_owner() {
        std::thread::yield_now();
    }
}

#[cfg(test)]
type AddressSpaceLock<T> = InterruptSpinLock<T, TestInterruptMask>;

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);

type SpaceResult<Backend, Account, Value> = Result<
    Value,
    AddressSpaceError<<Backend as PageBackend>::Error, <Account as MemoryAccount>::Error>,
>;
type MappingOwner<Account> = FallibleArc<MappingOwnership<<Account as MemoryAccount>::Charge>>;
type VmarOwner<Account> = FallibleArc<VmarOwnership<<Account as MemoryAccount>::Charge>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub(crate) struct AddressSpaceId(u64);

impl AddressSpaceId {
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Vmar {
    address_space: AddressSpaceId,
    id: u64,
    generation: u64,
    range: UserSlice,
}

impl Vmar {
    pub(crate) const fn range(self) -> UserSlice {
        self.range
    }
}

/// Bounded address-space observations; independent of object publication and ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmarDetailError {
    InvalidCursor,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VmarDetails {
    pub(crate) record: VmarDetailRecord,
    pub(crate) next_cursor: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmarDetailRecord {
    Empty,
    Region {
        range: UserSlice,
        live: bool,
    },
    Mapping {
        range: UserSlice,
        permissions: Permissions,
        maximum_permissions: Permissions,
    },
}

/// VMAR authority safe to publish through a Native application handle.
///
/// Internal VMARs retain the machine's complete user range. Keeping the
/// checked token in the handle object prevents root publication or delegation
/// from granting map, unmap, protect, or destroy authority over system pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ApplicationVmar(Vmar);

impl ApplicationVmar {
    pub(crate) const fn token(self) -> Vmar {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MappingToken {
    address_space: AddressSpaceId,
    base: UserAddress,
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MappingSnapshot {
    pub(crate) token: MappingToken,
    pub(crate) range: UserSlice,
    pub(crate) object_offset: u64,
    pub(crate) permissions: Permissions,
    pub(crate) executable_backing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MappingChange {
    pub(crate) address_space: AddressSpaceId,
    pub(crate) previous_epoch: u64,
    pub(crate) epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AddressSpaceError<BackendError, AccountError> {
    Account(AccountError),
    Allocation,
    BackingNotResident,
    BackingIo,
    Backend(BackendError),
    Busy,
    CowRequired,
    EmptyRange,
    IdentityExhausted,
    InvalidAddressSpace,
    InvalidPermissions,
    InvalidRange,
    NotMapped,
    Overlap,
    ReadDenied,
    SizeMismatch,
    SizeOverflow,
    StaleMapping,
    StaleTransaction,
    StaleVmar,
    Stopped,
    WriteDenied,
    WritableExecutableBacking,
}

impl<BackendError, AccountError> AddressSpaceError<BackendError, AccountError> {
    pub(crate) const fn is_stale_transaction(&self) -> bool {
        matches!(self, Self::StaleTransaction)
    }
}

impl<BackendError, AccountError> From<VmoError<BackendError, AccountError>>
    for AddressSpaceError<BackendError, AccountError>
{
    fn from(error: VmoError<BackendError, AccountError>) -> Self {
        match error {
            VmoError::Account(error) => Self::Account(error),
            VmoError::Allocation => Self::Allocation,
            VmoError::Backend(error) => Self::Backend(error),
            VmoError::Busy => Self::Busy,
            VmoError::CowRequired => Self::CowRequired,
            VmoError::BackingNotResident => Self::BackingNotResident,
            VmoError::BackingIo => Self::BackingIo,
            VmoError::InvalidRange => Self::InvalidRange,
            VmoError::SizeOverflow => Self::SizeOverflow,
        }
    }
}

struct MappingOwnership<Charge> {
    _charge: Charge,
}

struct Mapping<Backend: PageBackend, Account: MemoryAccount> {
    owner_vmar: u64,
    snapshot: MappingSnapshot,
    maximum_permissions: Permissions,
    object: MappingObject<Backend, Account>,
    ownership: FallibleArc<MappingOwnership<Account::Charge>>,
    write_lease: Option<WritableMappingLease<Backend, Account>>,
    locks: Option<FallibleArc<residency::LockedPages<Account>>>,
}

impl<Backend: PageBackend, Account: MemoryAccount> Clone for Mapping<Backend, Account> {
    fn clone(&self) -> Self {
        Self {
            owner_vmar: self.owner_vmar,
            snapshot: self.snapshot,
            maximum_permissions: self.maximum_permissions,
            object: self.object.clone(),
            ownership: self.ownership.clone(),
            write_lease: self.write_lease.clone(),
            locks: self.locks.clone(),
        }
    }
}

struct VmarOwnership<Charge> {
    _charge: Charge,
}

struct VmarRecord<Account: MemoryAccount> {
    token: Vmar,
    parent_id: u64,
    ownership: Option<VmarOwner<Account>>,
    free: FreeRanges<Account>,
}

impl<Account: MemoryAccount> Clone for VmarRecord<Account> {
    fn clone(&self) -> Self {
        Self {
            token: self.token,
            parent_id: self.parent_id,
            ownership: self.ownership.clone(),
            free: self.free.clone(),
        }
    }
}

impl<Account: MemoryAccount> Entry for VmarRecord<Account> {
    type Key = u64;
    type Weight = u64;
    fn key(&self) -> u64 {
        self.token.id
    }
}

type VmarSet<Account> = Index<VmarRecord<Account>, Account>;

struct MappingSet<Backend: PageBackend, Account: MemoryAccount> {
    records: MappingIndex<Backend, Account>,
    // Tree nodes carry their own charges across shared snapshot versions.
    _storage_charge: Option<Account::Charge>,
}

impl<BackendError, AccountError> From<index::Error<AccountError>>
    for AddressSpaceError<BackendError, AccountError>
{
    fn from(error: index::Error<AccountError>) -> Self {
        match error {
            index::Error::Account(error) => Self::Account(error),
            index::Error::Allocation => Self::Allocation,
        }
    }
}

#[derive(Clone, Copy)]
struct UserWriteRange {
    id: u64,
    range: UserSlice,
}

struct AddressSpaceState<Backend: PageBackend, Account: MemoryAccount> {
    sealed: bool,
    mappings: FallibleArc<MappingSet<Backend, Account>>,
    vmars: VmarSet<Account>,
    next_mapping_id: u64,
    next_vmar_id: u64,
    authority_epoch: u64,
    mapping_epoch: u64,
    user_writes: InlineBuffer<UserWriteRange>,
    user_write_charge: Option<Account::Charge>,
    next_user_write_id: u64,
}

struct AddressSpaceSnapshot<Backend: PageBackend, Account: MemoryAccount> {
    sealed: bool,
    mappings: FallibleArc<MappingSet<Backend, Account>>,
    vmars: VmarSet<Account>,
    authority_epoch: u64,
    mapping_epoch: u64,
}

pub(crate) struct UserAddressSpace<Backend: PageBackend, Account: MemoryAccount> {
    id: AddressSpaceId,
    root: Vmar,
    application_limit: u64,
    backend: Backend,
    account: Account,
    state: AddressSpaceLock<AddressSpaceState<Backend, Account>>,
    _metadata_charge: Account::Charge,
}

impl<Backend: PageBackend, Account: MemoryAccount> UserAddressSpace<Backend, Account> {
    pub(crate) fn try_new(
        window: UserAddressWindow,
        range: UserSlice,
        backend: Backend,
        account: Account,
    ) -> Result<Self, AddressSpaceError<Backend::Error, Account::Error>> {
        require_nonempty_aligned(range)?;
        if !window.range().contains(range) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let id = allocate_address_space_id()?;
        let bytes =
            u64::try_from(size_of::<Self>()).map_err(|_| AddressSpaceError::SizeOverflow)?;
        let metadata_charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes,
                kernel_objects: 1,
                address_spaces: 1,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        let mapping_set_charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: FallibleArc::<MappingSet<Backend, Account>>::allocation_size() as u64,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        let mappings = FallibleArc::try_new(MappingSet {
            records: MappingIndex::new(),
            _storage_charge: Some(mapping_set_charge),
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        let root = Vmar {
            address_space: id,
            id: 0,
            generation: 1,
            range,
        };
        let vmars = VmarSet::new().insert(
            VmarRecord {
                token: root,
                parent_id: 0,
                ownership: None,
                free: FreeRanges::new(range, &account)?,
            },
            &account,
        )?;
        Ok(Self {
            id,
            root,
            application_limit: window.application_limit(),
            backend,
            account,
            state: AddressSpaceLock::new(AddressSpaceState {
                sealed: false,
                mappings,
                vmars,
                next_mapping_id: 1,
                next_vmar_id: 1,
                authority_epoch: 1,
                mapping_epoch: 1,
                user_writes: InlineBuffer::default(),
                user_write_charge: None,
                next_user_write_id: 1,
            }),
            _metadata_charge: metadata_charge,
        })
    }

    pub(crate) fn vmar_details(
        &self,
        vmar: Vmar,
        cursor: u64,
    ) -> Result<VmarDetails, VmarDetailError> {
        let (live, mappings) = self.state.with(|state| {
            let live = vmar.address_space == self.id
                && if vmar.id == 0 {
                    vmar == self.root
                } else {
                    state
                        .vmars
                        .get(vmar.id)
                        .is_some_and(|record| record.token == vmar)
                };
            (live, state.mappings.clone())
        });
        if cursor == 0 {
            return Ok(VmarDetails {
                record: VmarDetailRecord::Region {
                    range: vmar.range,
                    live,
                },
                next_cursor: if live && !mappings.records.is_empty() {
                    1
                } else {
                    0
                },
            });
        }
        if !live {
            return Err(VmarDetailError::Stale);
        }
        let start = usize::try_from(cursor - 1).map_err(|_| VmarDetailError::InvalidCursor)?;
        if start > mappings.records.len() {
            return Err(VmarDetailError::InvalidCursor);
        }
        let end = start.saturating_add(32).min(mappings.records.len());
        let first = mappings.records.select(start).map(|mapping| mapping.key());
        for (offset, mapping) in first
            .into_iter()
            .flat_map(|key| mappings.records.iter_from(key))
            .take(end - start)
            .enumerate()
        {
            let index = start + offset;
            if !vmar.range.contains(mapping.snapshot.range) {
                continue;
            }
            return Ok(VmarDetails {
                record: VmarDetailRecord::Mapping {
                    range: mapping.snapshot.range,
                    permissions: mapping.snapshot.permissions,
                    maximum_permissions: mapping.maximum_permissions,
                },
                next_cursor: if index + 1 < mappings.records.len() {
                    index as u64 + 2
                } else {
                    0
                },
            });
        }
        Ok(VmarDetails {
            record: VmarDetailRecord::Empty,
            next_cursor: if end < mappings.records.len() {
                end as u64 + 1
            } else {
                0
            },
        })
    }

    pub(crate) const fn id(&self) -> AddressSpaceId {
        self.id
    }

    pub(crate) const fn root_vmar(&self) -> Vmar {
        self.root
    }

    /// Only this address-space owner may turn an internal VMAR into authority
    /// eligible for user publication, using its immutable profile boundary.
    pub(crate) fn application_vmar(&self, vmar: Vmar) -> Option<ApplicationVmar> {
        let range = vmar.range();
        if vmar.address_space != self.id
            || range.length() == 0
            || range.end().get() > self.application_limit
        {
            return None;
        }
        Some(ApplicationVmar(vmar))
    }

    pub(crate) fn mapping_epoch(&self) -> u64 {
        self.state.with(|state| state.mapping_epoch)
    }

    fn snapshot(&self) -> AddressSpaceSnapshot<Backend, Account> {
        self.state.with(|state| AddressSpaceSnapshot {
            sealed: state.sealed,
            mappings: state.mappings.clone(),
            vmars: state.vmars.clone(),
            authority_epoch: state.authority_epoch,
            mapping_epoch: state.mapping_epoch,
        })
    }

    pub(crate) fn try_create_vmar(
        &self,
        parent: Vmar,
        range: UserSlice,
    ) -> Result<Vmar, AddressSpaceError<Backend::Error, Account::Error>> {
        self.create_vmar(parent, Some(range), range.length(), 0)
    }

    /// Select and reserve a free range in the same optimistic transaction.
    pub(crate) fn try_create_vmar_hint(
        &self,
        parent: Vmar,
        size: u64,
        hint: u64,
    ) -> Result<Vmar, AddressSpaceError<Backend::Error, Account::Error>> {
        self.create_vmar(parent, None, size, hint)
    }

    fn create_vmar(
        &self,
        parent: Vmar,
        requested: Option<UserSlice>,
        size: u64,
        hint: u64,
    ) -> Result<Vmar, AddressSpaceError<Backend::Error, Account::Error>> {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) || !hint.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let snapshot = self.snapshot();
        let authority = validate_vmar(self.id, self.root, &snapshot, parent)?;
        let parent_record = snapshot
            .vmars
            .get(parent.id)
            .ok_or(AddressSpaceError::StaleVmar)?;
        let range = match requested {
            Some(range) => range,
            None => {
                let base = parent_record
                    .free
                    .nearest(size, hint)
                    .ok_or(AddressSpaceError::Allocation)?;
                UserSlice::new(UserAddress::new(base), size)
                    .map_err(|_| AddressSpaceError::InvalidRange)?
            }
        };
        require_nonempty_aligned(range)?;
        if !authority.contains(range) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let free = parent_record
            .free
            .reserve(range, &self.account)?
            .ok_or(AddressSpaceError::Overlap)?;
        let next_epoch = snapshot
            .authority_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let id = self.state.with(|state| {
            if state.sealed {
                return Err(AddressSpaceError::Stopped);
            }
            if state.authority_epoch != snapshot.authority_epoch {
                return Err(AddressSpaceError::StaleTransaction);
            }
            let id = state.next_vmar_id;
            let next = id
                .checked_add(1)
                .ok_or(AddressSpaceError::IdentityExhausted)?;
            state.next_vmar_id = next;
            Ok(id)
        })?;
        let token = Vmar {
            address_space: self.id,
            id,
            generation: next_epoch,
            range,
        };
        let child = VmarRecord {
            token,
            parent_id: parent.id,
            ownership: Some(self.try_charge_vmar_record()?),
            free: FreeRanges::new(range, &self.account)?,
        };
        let mut updated_parent = parent_record.clone();
        updated_parent.free = free;
        let replacement = snapshot
            .vmars
            .insert(updated_parent, &self.account)?
            .insert(child, &self.account)?;
        let retired = self.state.with(|state| {
            if state.sealed {
                return Err(AddressSpaceError::Stopped);
            }
            if state.authority_epoch != snapshot.authority_epoch {
                return Err(AddressSpaceError::StaleTransaction);
            }
            // Cloning the already-owned set is infallible and ensures a stale
            // transaction destroys its prepared allocation after this IRQ-safe
            // lock has been released.
            let old = core::mem::replace(&mut state.vmars, replacement.clone());
            state.authority_epoch = next_epoch;
            Ok(old)
        })?;
        drop(retired);
        Ok(token)
    }

    pub(crate) fn destroy_vmar(
        &self,
        token: Vmar,
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        if token.id == 0 {
            return Err(AddressSpaceError::InvalidRange);
        }
        let snapshot = self.snapshot();
        validate_vmar(self.id, self.root, &snapshot, token)?;
        let record = snapshot
            .vmars
            .get(token.id)
            .ok_or(AddressSpaceError::StaleVmar)?;
        if !record.free.contains(token.range) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let next_epoch = snapshot
            .authority_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let mut parent = snapshot
            .vmars
            .get(record.parent_id)
            .ok_or(AddressSpaceError::StaleVmar)?
            .clone();
        parent.free = parent
            .free
            .release(token.range, &self.account)?
            .ok_or(AddressSpaceError::Overlap)?;
        let replacement = snapshot
            .vmars
            .remove(token.id, &self.account)?
            .insert(parent, &self.account)?;
        let retired = self.state.with(|state| {
            if state.sealed {
                return Err(AddressSpaceError::Stopped);
            }
            if state.authority_epoch != snapshot.authority_epoch {
                return Err(AddressSpaceError::StaleTransaction);
            }
            let old = core::mem::replace(&mut state.vmars, replacement.clone());
            state.authority_epoch = next_epoch;
            Ok(old)
        })?;
        drop(retired);
        Ok(())
    }

    pub(crate) fn is_private_write(&self, address: UserAddress) -> bool {
        let snapshot = self.snapshot();
        snapshot
            .mappings
            .records
            .containing(address)
            .is_some_and(|mapping| {
                mapping.snapshot.permissions.contains(Access::Write)
                    && mapping.object.private().is_some()
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_map_snapshot(
        &self,
        vmar: Vmar,
        range: UserSlice,
        source: SnapshotVmo<Backend, Account>,
        offset: u64,
        permissions: Permissions,
        maximum: Permissions,
    ) -> SpaceResult<Backend, Account, PreparedMappingChange<'_, Backend, Account>> {
        self.prepare_map(
            vmar,
            range,
            MappingObject::Snapshot(source),
            offset,
            permissions,
            maximum,
        )
    }

    pub(super) fn prepare_private_view_builder(
        &self,
        source: SnapshotVmo<Backend, Account>,
        size: u64,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        mode: PrivateMappingMode,
    ) -> SpaceResult<Backend, Account, super::vmo::PrivateViewBuilder<Backend, Account>> {
        Ok(super::vmo::PrivateViewBuilder::try_new(
            source,
            size,
            source_offset,
            source_length,
            data_offset,
            mode,
            self.backend.clone(),
            self.account.clone(),
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_map_private(
        &self,
        vmar: Vmar,
        range: UserSlice,
        source: SnapshotVmo<Backend, Account>,
        source_offset: u64,
        source_length: u64,
        data_offset: u64,
        permissions: Permissions,
        maximum: Permissions,
        mode: PrivateMappingMode,
    ) -> SpaceResult<Backend, Account, PreparedMappingChange<'_, Backend, Account>> {
        require_nonempty_aligned(range)?;
        let view = PrivateView::try_new(
            source,
            range.length(),
            source_offset,
            source_length,
            data_offset,
            mode,
            self.backend.clone(),
            self.account.clone(),
        )?;
        self.prepare_map_private_view(vmar, range, view, permissions, maximum)
    }

    pub(crate) fn prepare_map_private_view(
        &self,
        vmar: Vmar,
        range: UserSlice,
        view: FallibleArc<PrivateView<Backend, Account>>,
        permissions: Permissions,
        maximum: Permissions,
    ) -> SpaceResult<Backend, Account, PreparedMappingChange<'_, Backend, Account>> {
        self.prepare_map(
            vmar,
            range,
            MappingObject::Private(view),
            0,
            permissions,
            maximum,
        )
    }

    /// Builds one transactional replacement of all private versions touched by
    /// a write. Concrete retired page owners remain in the prior mapping set
    /// until the machine acknowledges its translation epoch's retirement.
    pub(crate) fn prepare_private_write(
        &self,
        range: UserSlice,
    ) -> SpaceResult<Backend, Account, Option<PreparedMappingChange<'_, Backend, Account>>> {
        if !self.root.range.contains(range) {
            return Err(AddressSpaceError::InvalidRange);
        }
        if range.length() == 0 {
            return Ok(None);
        }
        let snapshot = self.snapshot();
        let mut cursor = range.base();
        if snapshot.sealed {
            return Err(AddressSpaceError::Stopped);
        }
        let mut needed = false;
        for mapping in snapshot.mappings.records.overlapping(range) {
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                continue;
            };
            if overlap.base() != cursor {
                return Err(AddressSpaceError::NotMapped);
            }
            if !mapping.snapshot.permissions.contains(Access::Write) {
                return Err(AddressSpaceError::WriteDenied);
            }
            let offset = mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get();
            if !mapping.object.writable_backing(
                offset,
                usize::try_from(overlap.length()).map_err(|_| AddressSpaceError::SizeOverflow)?,
            )? {
                needed = true;
            }
            cursor = overlap.end();
        }
        if cursor != range.end() {
            return Err(AddressSpaceError::NotMapped);
        }
        if !needed {
            return Ok(None);
        }
        let storage_charge = self.try_mapping_set_charge()?;
        let mut replacement = snapshot.mappings.records.clone();
        // A view may occur in several protect/unmap fragments. Each prepared
        // version replaces every occurrence so one logical alias cannot retain
        // different bytes. Mapping tokens deliberately survive COW; futex
        // identities are virtual mappings, not physical page identities.
        for original in snapshot.mappings.records.overlapping(range) {
            let mapping = replacement
                .get(original.key())
                .ok_or(AddressSpaceError::StaleMapping)?;
            let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                continue;
            };
            let Some(view) = mapping.object.private() else {
                continue;
            };
            let offset = mapping.snapshot.object_offset + overlap.base().get()
                - mapping.snapshot.range.base().get();
            let first =
                usize::try_from(offset / PAGE_SIZE).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let end = offset
                .checked_add(overlap.length())
                .and_then(|end| end.checked_add(PAGE_SIZE - 1))
                .ok_or(AddressSpaceError::SizeOverflow)?
                / PAGE_SIZE;
            let count = usize::try_from(end).map_err(|_| AddressSpaceError::SizeOverflow)? - first;
            if (first..first + count).all(|page| view.is_writable(page)) {
                continue;
            }
            let old = view.clone();
            let changed = view.materialize(first, count)?;
            replacement.replace_private_view(&old, &changed, &self.account)?;
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

    pub(crate) fn prepare_map_writable(
        &self,
        vmar: Vmar,
        range: UserSlice,
        object: WritableVmo<Backend, Account>,
        object_offset: u64,
        permissions: Permissions,
        maximum_permissions: Permissions,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        self.prepare_map(
            vmar,
            range,
            MappingObject::Writable(object),
            object_offset,
            permissions,
            maximum_permissions,
        )
    }

    pub(crate) fn prepare_map_executable(
        &self,
        vmar: Vmar,
        range: UserSlice,
        object: ExecutableVmo<Backend, Account>,
        object_offset: u64,
        permissions: Permissions,
        maximum_permissions: Permissions,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        self.prepare_map(
            vmar,
            range,
            MappingObject::Executable(object),
            object_offset,
            permissions,
            maximum_permissions,
        )
    }

    fn prepare_map(
        &self,
        vmar: Vmar,
        range: UserSlice,
        object: MappingObject<Backend, Account>,
        object_offset: u64,
        permissions: Permissions,
        maximum_permissions: Permissions,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        require_nonempty_aligned(range)?;
        require_page_aligned(object_offset)?;
        validate_mapping_permissions(&object, permissions)?;
        validate_mapping_permissions(&object, maximum_permissions)?;
        if maximum_permissions == Permissions::NONE {
            return Err(AddressSpaceError::InvalidPermissions);
        }
        if !permissions.is_subset_of(maximum_permissions) {
            return Err(AddressSpaceError::InvalidPermissions);
        }
        let object_end = object_offset
            .checked_add(range.length())
            .ok_or(AddressSpaceError::SizeOverflow)?;
        if object_end > object.size() {
            return Err(AddressSpaceError::InvalidRange);
        }
        let snapshot = self.snapshot();
        validate_mapping_range(self.id, self.root, &snapshot, vmar, range)?;
        let next_authority_epoch = snapshot
            .authority_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let next_mapping_epoch = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let id = self.state.with(|state| {
            if state.authority_epoch != snapshot.authority_epoch {
                return Err(AddressSpaceError::StaleTransaction);
            }
            reserve_mapping_ids(state, 1)
        })?;
        let token = MappingToken {
            address_space: self.id,
            base: range.base(),
            id,
            generation: next_authority_epoch,
        };

        if !object.fully_resident(object_offset, range.length())? {
            return Err(AddressSpaceError::BackingNotResident);
        }
        let ownership = self.try_mapping_ownership()?;
        let write_lease = if permissions.contains(Access::Write) && object.private().is_none() {
            Some(object.try_write_lease()?)
        } else {
            None
        };
        let storage_charge = self.try_mapping_set_charge()?;
        let replacement = snapshot.mappings.records.insert(
            Mapping {
                owner_vmar: vmar.id,
                snapshot: MappingSnapshot {
                    token,
                    range,
                    object_offset,
                    permissions,
                    executable_backing: object.executable(),
                },
                maximum_permissions,
                object,
                ownership,
                write_lease,
                locks: None,
            },
            &self.account,
        )?;
        let replacement = FallibleArc::try_new(MappingSet {
            records: replacement,
            _storage_charge: storage_charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        PreparedMappingChange::new(
            self,
            snapshot.authority_epoch,
            next_authority_epoch,
            snapshot.mapping_epoch,
            next_mapping_epoch,
            (
                replacement,
                self.mapping_vmar_update(&snapshot, vmar, range, true)?,
            ),
            range,
        )
    }

    fn mapping_vmar_update(
        &self,
        snapshot: &AddressSpaceSnapshot<Backend, Account>,
        vmar: Vmar,
        range: UserSlice,
        reserve: bool,
    ) -> SpaceResult<Backend, Account, VmarSet<Account>> {
        let mut record = snapshot
            .vmars
            .get(vmar.id)
            .ok_or(AddressSpaceError::StaleVmar)?
            .clone();
        record.free = if reserve {
            record.free.reserve(range, &self.account)?
        } else {
            record.free.release(range, &self.account)?
        }
        .ok_or(AddressSpaceError::Overlap)?;
        Ok(snapshot.vmars.insert(record, &self.account)?)
    }

    pub(crate) fn prepare_unmap(
        &self,
        vmar: Vmar,
        range: UserSlice,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        self.prepare_rewrite(vmar, range, None)
    }

    pub(crate) fn prepare_protect(
        &self,
        vmar: Vmar,
        range: UserSlice,
        permissions: Permissions,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        if !permissions.is_valid() {
            return Err(AddressSpaceError::InvalidPermissions);
        }
        self.prepare_rewrite(vmar, range, Some(permissions))
    }

    fn prepare_rewrite(
        &self,
        vmar: Vmar,
        range: UserSlice,
        permissions: Option<Permissions>,
    ) -> Result<
        PreparedMappingChange<'_, Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        require_nonempty_aligned(range)?;
        let snapshot = self.snapshot();
        validate_owned_mapped_range(self.id, self.root, &snapshot, vmar, range)?;
        let generation = snapshot
            .authority_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let next_mapping_epoch = snapshot
            .mapping_epoch
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let rewritten = {
            let mut rewritten = 0usize;
            for mapping in snapshot.mappings.records.overlapping(range) {
                if let Some(new_permissions) = permissions {
                    validate_mapping_permissions(&mapping.object, new_permissions)?;
                    if !new_permissions.is_subset_of(mapping.maximum_permissions) {
                        return Err(AddressSpaceError::InvalidPermissions);
                    }
                }
                let fragments =
                    fragment_total(mapping.snapshot.range, range, permissions.is_some());
                rewritten = rewritten
                    .checked_add(fragments)
                    .ok_or(AddressSpaceError::SizeOverflow)?;
            }
            rewritten
        };
        let first_token = self.state.with(|state| {
            if state.authority_epoch != snapshot.authority_epoch {
                return Err(AddressSpaceError::StaleTransaction);
            }
            reserve_mapping_ids(state, rewritten)
        })?;

        let storage_charge = self.try_mapping_set_charge()?;
        let mut replacement = snapshot.mappings.records.clone();

        let mut next_token = first_token;
        for mapping in snapshot.mappings.records.overlapping(range) {
            replacement = replacement.remove(mapping.key(), &self.account)?;
            append_fragments(
                self,
                &mut replacement,
                mapping,
                range,
                permissions,
                generation,
                &mut next_token,
            )?;
        }
        if permissions.is_none() {
            prune_private_views(
                &mut replacement,
                snapshot.mappings.records.overlapping(range),
                &self.account,
            )?;
        }
        let replacement = FallibleArc::try_new(MappingSet {
            records: replacement,
            _storage_charge: storage_charge,
        })
        .map_err(|_| AddressSpaceError::Allocation)?;
        PreparedMappingChange::new(
            self,
            snapshot.authority_epoch,
            generation,
            snapshot.mapping_epoch,
            next_mapping_epoch,
            (
                replacement,
                if permissions.is_none() {
                    self.mapping_vmar_update(&snapshot, vmar, range, false)?
                } else {
                    snapshot.vmars.clone()
                },
            ),
            range,
        )
    }

    #[cfg(test)]
    pub(crate) fn mapping_snapshot(
        &self,
        token: MappingToken,
    ) -> Result<MappingSnapshot, AddressSpaceError<Backend::Error, Account::Error>> {
        if token.address_space != self.id {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        let snapshot = self.snapshot();
        snapshot
            .mappings
            .records
            .by_token(token)
            .map(|mapping| mapping.snapshot)
            .ok_or(AddressSpaceError::StaleMapping)
    }

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
        let plan = self.prepare_copy(source, Access::Read)?;
        let mut copied = 0usize;
        for segment in plan.segments.iter() {
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let end = copied
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            let target = destination
                .get_mut(copied..end)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            segment.object.read_exposed(segment.object_offset, target)?;
            copied = end;
        }
        Ok(())
    }

    /// Resolves and retains every source page before a caller acquires I/O
    /// serialization locks. A successful plan never faults on later copies.
    pub(crate) fn prepare_user_read(
        &self,
        source: UserSlice,
    ) -> SpaceResult<Backend, Account, PreparedUserRead<Backend, Account>> {
        let mut plan = self.prepare_copy(source, Access::Read)?;
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
        let plan = self.prepare_copy(destination, Access::Write)?;
        let mut copied = 0usize;
        for segment in plan.segments.iter() {
            let length =
                usize::try_from(segment.length).map_err(|_| AddressSpaceError::SizeOverflow)?;
            let end = copied
                .checked_add(length)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            let source = source
                .get(copied..end)
                .ok_or(AddressSpaceError::SizeOverflow)?;
            segment
                .object
                .write_exposed(segment.object_offset, source)?;
            copied = end;
        }
        Ok(())
    }

    /// Pins the current writable mapping set for a later transactional copy.
    ///
    /// While the returned plan is active, authority changes intersecting its
    /// exact byte range return `Busy`; unrelated mappings may still change.
    /// This closes the interval between writing future capability values and
    /// publishing those values in a Process handle table: another Thread
    /// cannot redirect the output range to unrelated backing in that window.
    pub(super) fn prepare_user_write(
        &self,
        destination: UserSlice,
    ) -> Result<
        PreparedUserWrite<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        let snapshot = self.snapshot();
        let mut plan = self.prepare_copy_from_snapshot(destination, Access::Write, &snapshot)?;
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

    pub(super) fn write_user_reservation(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_from(source)
    }

    pub(super) fn write_user_reservation_prefix(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        source: &[u8],
    ) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_prefix_from(source)
    }

    pub(super) fn write_user_reservation_parts<'a>(
        &self,
        reservation: &PreparedUserWrite<Backend, Account>,
        parts: impl Iterator<Item = &'a [u8]> + Clone,
    ) -> SpaceResult<Backend, Account, ()> {
        if reservation.address_space != self.id || reservation.completed {
            return Err(AddressSpaceError::InvalidAddressSpace);
        }
        reservation.plan.copy_parts_from(parts)
    }

    pub(super) fn release_user_write(&self, mut reservation: PreparedUserWrite<Backend, Account>) {
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

    /// Resolves process-private wait identity without touching or pinning data.
    /// A concurrent unmap may retire this mapping, but its never-reused token
    /// can only match waiters on that same old mapping, never its replacement.
    pub(crate) fn atomic_wait_key(
        &self,
        address: UserAddress,
    ) -> SpaceResult<Backend, Account, MappingToken> {
        let snapshot = self.snapshot();
        Ok(Self::atomic_mapping(&snapshot, address)?.snapshot.token)
    }

    fn atomic_mapping(
        snapshot: &AddressSpaceSnapshot<Backend, Account>,
        address: UserAddress,
    ) -> SpaceResult<Backend, Account, &Mapping<Backend, Account>> {
        if !address.get().is_multiple_of(4) {
            return Err(AddressSpaceError::InvalidRange);
        }
        let range = UserSlice::new(address, 4).map_err(|_| AddressSpaceError::InvalidRange)?;
        if snapshot.sealed {
            return Err(AddressSpaceError::Stopped);
        }
        let mapping = snapshot
            .mappings
            .records
            .containing(address)
            .filter(|mapping| mapping.snapshot.range.contains(range))
            .ok_or(AddressSpaceError::NotMapped)?;
        if !mapping.snapshot.permissions.contains(Access::Write) {
            return Err(AddressSpaceError::WriteDenied);
        }
        Ok(mapping)
    }

    /// Pins one writable atomic word without ordinary byte copies. The mapping
    /// token never aliases a replacement mapping at the same virtual address.
    pub(crate) fn pin_atomic_u32(
        &self,
        address: UserAddress,
    ) -> Result<PinnedAtomicWord<Backend, Account>, AddressSpaceError<Backend::Error, Account::Error>>
    {
        let snapshot = self.snapshot();
        let mapping = Self::atomic_mapping(&snapshot, address)?;
        let offset =
            mapping.snapshot.object_offset + address.get() - mapping.snapshot.range.base().get();
        if !mapping.object.writable_backing(offset, 4)? {
            return Err(AddressSpaceError::CowRequired);
        }
        let physical = mapping
            .object
            .physical_page(offset / PAGE_SIZE * PAGE_SIZE)?;
        let charge = self
            .account
            .try_charge(MemoryCharge {
                pinned_pages: 1,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        Ok(PinnedAtomicWord {
            token: mapping.snapshot.token,
            physical: physical.get() + offset % PAGE_SIZE,
            _backing: match mapping.object.pin_private_page(offset)? {
                Some(page) => PinnedWordBacking::Private { _page: page },
                None => PinnedWordBacking::Shared {
                    _mapping: mapping.clone(),
                },
            },
            _charge: charge,
        })
    }

    fn prepare_copy(
        &self,
        range: UserSlice,
        access: Access,
    ) -> Result<CopyPlan<Backend, Account>, AddressSpaceError<Backend::Error, Account::Error>> {
        let snapshot = self.snapshot();
        self.prepare_copy_from_snapshot(range, access, &snapshot)
    }

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
                && !mapping.object.writable_backing(
                    object_offset,
                    usize::try_from(overlap.length())
                        .map_err(|_| AddressSpaceError::SizeOverflow)?,
                )?
            {
                return Err(AddressSpaceError::CowRequired);
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
            _temporary_charge: temporary_charge,
        })
    }

    fn try_mapping_ownership(&self) -> SpaceResult<Backend, Account, MappingOwner<Account>> {
        let charge = self
            .account
            .try_charge(MemoryCharge {
                kernel_bytes: FallibleArc::<MappingOwnership<Account::Charge>>::allocation_size()
                    as u64,
                mappings: 1,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        FallibleArc::try_new(MappingOwnership { _charge: charge })
            .map_err(|_| AddressSpaceError::Allocation)
    }

    fn try_charge_vmar_record(&self) -> SpaceResult<Backend, Account, VmarOwner<Account>> {
        let charge = self
            .account
            .try_charge(MemoryCharge {
                kernel_bytes: FallibleArc::<VmarOwnership<Account::Charge>>::allocation_size()
                    as u64,
                kernel_objects: 1,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        FallibleArc::try_new(VmarOwnership { _charge: charge })
            .map_err(|_| AddressSpaceError::Allocation)
    }

    fn try_storage_charge<Item>(
        &self,
        capacity: usize,
    ) -> SpaceResult<Backend, Account, Option<Account::Charge>> {
        if capacity == 0 {
            return Ok(None);
        }
        let bytes = capacity
            .checked_mul(size_of::<Item>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(AddressSpaceError::SizeOverflow)?;
        self.account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes,
                ..MemoryCharge::default()
            })
            .map(Some)
            .map_err(AddressSpaceError::Account)
    }

    fn try_mapping_set_charge(&self) -> SpaceResult<Backend, Account, Option<Account::Charge>> {
        self.account
            .try_charge(MemoryCharge {
                kernel_bytes: FallibleArc::<MappingSet<Backend, Account>>::allocation_size() as u64,
                ..MemoryCharge::default()
            })
            .map(Some)
            .map_err(AddressSpaceError::Account)
    }
}

#[cold]
fn address_space_invariant_violation() -> ! {
    hyper::debug::invariant_failure("user address space invariant")
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MappingChangeKind {
    Authority {
        range: UserSlice,
    },
    /// Changes only previously read-only private physical backing. Existing
    /// writable pages, virtual ranges, permissions and mapping tokens survive.
    PrivateBacking,
    /// Drops only clean file pages, including in a sealed stopped space.
    FileReclaim,
}

#[must_use = "prepared mapping state must be committed or explicitly abandoned"]
pub(crate) struct PreparedMappingChange<'a, Backend: PageBackend, Account: MemoryAccount> {
    address_space: &'a UserAddressSpace<Backend, Account>,
    base_authority_epoch: u64,
    next_authority_epoch: u64,
    base_mapping_epoch: u64,
    next_mapping_epoch: u64,
    replacement: FallibleArc<MappingSet<Backend, Account>>,
    replacement_vmars: Option<VmarSet<Account>>,
    kind: MappingChangeKind,
}

impl<'a, Backend: PageBackend, Account: MemoryAccount> PreparedMappingChange<'a, Backend, Account> {
    fn new(
        address_space: &'a UserAddressSpace<Backend, Account>,
        base_authority_epoch: u64,
        next_authority_epoch: u64,
        base_mapping_epoch: u64,
        next_mapping_epoch: u64,
        replacement: (FallibleArc<MappingSet<Backend, Account>>, VmarSet<Account>),
        range: UserSlice,
    ) -> Result<Self, AddressSpaceError<Backend::Error, Account::Error>> {
        Ok(Self {
            address_space,
            base_authority_epoch,
            next_authority_epoch,
            base_mapping_epoch,
            next_mapping_epoch,
            replacement: replacement.0,
            replacement_vmars: Some(replacement.1),
            kind: MappingChangeKind::Authority { range },
        })
    }

    /// Backing/residency changes preserve every mapping's authority and every
    /// already-writable page owner. A reserved
    /// user output is fully materialized before reservation publication, so no
    /// page referenced by an active reservation can change in this transaction.
    fn new_private_backing(
        address_space: &'a UserAddressSpace<Backend, Account>,
        authority_epoch: u64,
        base_mapping_epoch: u64,
        next_mapping_epoch: u64,
        replacement: FallibleArc<MappingSet<Backend, Account>>,
    ) -> Result<Self, AddressSpaceError<Backend::Error, Account::Error>> {
        Ok(Self {
            address_space,
            base_authority_epoch: authority_epoch,
            next_authority_epoch: authority_epoch,
            base_mapping_epoch,
            next_mapping_epoch,
            replacement,
            replacement_vmars: None,
            kind: MappingChangeKind::PrivateBacking,
        })
    }

    pub(crate) fn publish_file_instructions(
        &self,
        context: &Backend::InstructionPublicationContext,
    ) -> SpaceResult<Backend, Account, ()> {
        for mapping in self.replacement.records.iter() {
            if let Some(view) = mapping.object.private() {
                view.publish_file_pages(context)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn snapshots(&self) -> impl Iterator<Item = MappingSnapshot> + '_ {
        self.replacement
            .records
            .iter()
            .map(|mapping| mapping.snapshot)
    }

    pub(super) const fn base_epoch(&self) -> u64 {
        self.base_mapping_epoch
    }

    pub(super) const fn next_epoch(&self) -> u64 {
        self.next_mapping_epoch
    }

    #[cfg(test)]
    pub(crate) fn resident_pages(
        &self,
        token: MappingToken,
    ) -> Result<
        PreparedPageSnapshot<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        let mapping = self
            .replacement
            .records
            .by_token(token)
            .ok_or(AddressSpaceError::StaleMapping)?;
        let count = usize::try_from(mapping.snapshot.range.length() / PAGE_SIZE)
            .map_err(|_| AddressSpaceError::SizeOverflow)?;
        let charge = self
            .address_space
            .try_storage_charge::<Option<PhysicalAddress>>(count)?;
        let pages = super::vmo::resident_pages(
            &mapping.object,
            mapping.snapshot.object_offset,
            mapping.snapshot.range.length(),
        )?;
        Ok(PreparedPageSnapshot {
            pages,
            _charge: charge,
            mapping: mapping.clone(),
            _pins: self.replacement.clone(),
        })
    }

    /// Publishes the logical set only if no intervening mapping/VMAR commit won.
    pub(super) fn commit_machine(
        self,
    ) -> Result<
        CommittedMappingChange<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        let (retired, retired_vmars) = self.address_space.state.with(|state| {
            if state.sealed && !matches!(self.kind, MappingChangeKind::FileReclaim) {
                return Err(AddressSpaceError::Stopped);
            }
            if state.authority_epoch != self.base_authority_epoch
                || state.mapping_epoch != self.base_mapping_epoch
            {
                return Err(AddressSpaceError::StaleTransaction);
            }
            if let MappingChangeKind::Authority { range } = self.kind
                && state
                    .user_writes
                    .iter()
                    .any(|record| record.range.length() != 0 && ranges_overlap(record.range, range))
            {
                return Err(AddressSpaceError::Busy);
            }
            // Keep the prepared owner outside the lock so a stale commit never
            // runs mapping/page/account destructors with local IRQs masked.
            let old = core::mem::replace(&mut state.mappings, self.replacement.clone());
            state.authority_epoch = self.next_authority_epoch;
            state.mapping_epoch = self.next_mapping_epoch;
            let old_vmars = self
                .replacement_vmars
                .as_ref()
                .map(|replacement| core::mem::replace(&mut state.vmars, replacement.clone()));
            Ok((old, old_vmars))
        })?;
        // Authority metadata has no hardware residency. Release displaced
        // path nodes outside the IRQ-safe lock; mapping owners still wait for
        // the machine retirement acknowledgement below.
        drop(retired_vmars);
        Ok(CommittedMappingChange {
            change: MappingChange {
                address_space: self.address_space.id,
                previous_epoch: self.base_mapping_epoch,
                epoch: self.next_mapping_epoch,
            },
            retired: ManuallyDrop::new(retired),
        })
    }

    #[cfg(any(test, all(feature = "kernel-test", feature = "kernel-self-test")))]
    #[allow(
        dead_code,
        reason = "shared test helper; hardware coverage is architecture-dependent"
    )]
    pub(crate) fn commit_for_test(
        self,
    ) -> Result<
        CommittedMappingChange<Backend, Account>,
        AddressSpaceError<Backend::Error, Account::Error>,
    > {
        self.commit_machine()
    }
}

#[cfg(test)]
pub(crate) struct PreparedPageSnapshot<Backend: PageBackend, Account: MemoryAccount> {
    pages: Vec<Option<PhysicalAddress>>,
    _charge: Option<Account::Charge>,
    mapping: Mapping<Backend, Account>,
    _pins: FallibleArc<MappingSet<Backend, Account>>,
}

#[cfg(test)]
impl<Backend: PageBackend, Account: MemoryAccount> PreparedPageSnapshot<Backend, Account> {
    pub(crate) fn page_is_writable(&self, index: usize) -> bool {
        let mapping = &self.mapping;
        let Some(offset) = (index as u64)
            .checked_mul(PAGE_SIZE)
            .and_then(|delta| mapping.snapshot.object_offset.checked_add(delta))
        else {
            return false;
        };
        index < self.pages.len()
            && mapping
                .object
                .writable_backing(offset, PAGE_SIZE as usize)
                .unwrap_or(false)
    }

    pub(crate) fn pages(&self) -> &[Option<PhysicalAddress>] {
        &self.pages
    }
}

/// Published logical state whose prior physical owners await invalidation.
#[must_use = "retired mappings remain pinned until invalidation is acknowledged"]
pub(crate) struct CommittedMappingChange<Backend: PageBackend, Account: MemoryAccount> {
    change: MappingChange,
    retired: ManuallyDrop<FallibleArc<MappingSet<Backend, Account>>>,
}

impl<Backend: PageBackend, Account: MemoryAccount> CommittedMappingChange<Backend, Account> {
    pub(crate) const fn change(&self) -> MappingChange {
        self.change
    }

    /// Releases old backing after every machine translation is quiescent.
    ///
    /// # Safety
    ///
    /// No CPU may retain an active or cached translation derived from the
    /// previous epoch. Stage3 must establish that through its opaque
    /// invalidation acknowledgement before calling this method.
    pub(super) unsafe fn complete_machine_retirement(mut self) {
        // SAFETY: The caller proves the exact translation-quiescence condition
        // required before old mapping owners and their pages may be reclaimed.
        unsafe { ManuallyDrop::drop(&mut self.retired) };
    }

    #[cfg(any(test, all(feature = "kernel-test", feature = "kernel-self-test")))]
    #[allow(
        dead_code,
        reason = "shared test helper; hardware coverage is architecture-dependent"
    )]
    pub(crate) unsafe fn complete_retirement_for_test(mut self) {
        // SAFETY: Test callers model the required acknowledged quiescence.
        unsafe { ManuallyDrop::drop(&mut self.retired) };
    }
}

enum CopyBacking<B: PageBackend, A: MemoryAccount> {
    Mapping(MappingObject<B, A>),
    Private(super::vmo::PinnedPrivateRange<B, A>),
    Read(super::vmo::PinnedReadRange<B, A>),
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
            segment.object.write_exposed(segment.object_offset, bytes)?;
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
                segment
                    .object
                    .write_exposed(segment.object_offset + copied, &source[..count])?;
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
            segment.object.write_exposed(segment.object_offset, bytes)?;
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

impl<Backend: PageBackend, Account: MemoryAccount> Drop for PreparedUserWrite<Backend, Account> {
    fn drop(&mut self) {
        if !self.completed {
            address_space_invariant_violation();
        }
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> ForeignMemory
    for UserAddressSpace<Backend, Account>
{
    type Error = AddressSpaceError<Backend::Error, Account::Error>;

    fn address_base(&self) -> u64 {
        self.root.range.base().get()
    }

    fn address_size(&self) -> u64 {
        self.root.range.length()
    }

    fn page_size(&self) -> usize {
        PAGE_SIZE as usize
    }

    fn read_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error> {
        let range = self.page_chunk(page_index, page_offset, destination.len())?;
        self.copy_from_user(range, destination)
    }

    fn write_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error> {
        let range = self.page_chunk(page_index, page_offset, source.len())?;
        self.copy_to_user(range, source)
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> UserAddressSpace<Backend, Account> {
    fn page_chunk(
        &self,
        page_index: usize,
        page_offset: usize,
        length: usize,
    ) -> Result<UserSlice, AddressSpaceError<Backend::Error, Account::Error>> {
        let page_index = u64::try_from(page_index).map_err(|_| AddressSpaceError::SizeOverflow)?;
        let page_offset =
            u64::try_from(page_offset).map_err(|_| AddressSpaceError::SizeOverflow)?;
        let length = u64::try_from(length).map_err(|_| AddressSpaceError::SizeOverflow)?;
        let offset = page_index
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let address = self
            .root
            .range
            .base()
            .checked_add(offset)
            .ok_or(AddressSpaceError::SizeOverflow)?;
        UserSlice::new(address, length).map_err(|_| AddressSpaceError::SizeOverflow)
    }
}

fn allocate_address_space_id<BackendError, AccountError>()
-> Result<AddressSpaceId, AddressSpaceError<BackendError, AccountError>> {
    let mut current = NEXT_ADDRESS_SPACE_ID.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return Err(AddressSpaceError::IdentityExhausted);
        }
        let next = current
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        match NEXT_ADDRESS_SPACE_ID.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(AddressSpaceId(current)),
            Err(observed) => current = observed,
        }
    }
}

fn validate_vmar<Backend: PageBackend, Account: MemoryAccount>(
    address_space: AddressSpaceId,
    root: Vmar,
    state: &AddressSpaceSnapshot<Backend, Account>,
    token: Vmar,
) -> Result<UserSlice, AddressSpaceError<Backend::Error, Account::Error>> {
    if state.sealed {
        return Err(AddressSpaceError::Stopped);
    }
    if token.address_space != address_space {
        return Err(AddressSpaceError::InvalidAddressSpace);
    }
    if token.id == 0 {
        return if token == root {
            Ok(root.range)
        } else {
            Err(AddressSpaceError::StaleVmar)
        };
    }
    state
        .vmars
        .get(token.id)
        .filter(|record| record.token == token)
        .map(|record| record.token.range)
        .ok_or(AddressSpaceError::StaleVmar)
}

fn validate_mapping_range<Backend: PageBackend, Account: MemoryAccount>(
    address_space: AddressSpaceId,
    root: Vmar,
    state: &AddressSpaceSnapshot<Backend, Account>,
    vmar: Vmar,
    range: UserSlice,
) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
    let authority = validate_vmar(address_space, root, state, vmar)?;
    if !authority.contains(range) {
        return Err(AddressSpaceError::InvalidRange);
    }
    if !state
        .vmars
        .get(vmar.id)
        .is_some_and(|record| record.free.contains(range))
    {
        return Err(AddressSpaceError::Overlap);
    }
    Ok(())
}

fn validate_owned_mapped_range<Backend: PageBackend, Account: MemoryAccount>(
    address_space: AddressSpaceId,
    root: Vmar,
    state: &AddressSpaceSnapshot<Backend, Account>,
    vmar: Vmar,
    range: UserSlice,
) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
    let authority = validate_vmar(address_space, root, state, vmar)?;
    if !authority.contains(range) {
        return Err(AddressSpaceError::NotMapped);
    }
    let mut cursor = range.base();
    for mapping in state.mappings.records.overlapping(range) {
        if mapping.owner_vmar != vmar.id {
            continue;
        }
        let Some(overlap) = intersection(mapping.snapshot.range, range) else {
            continue;
        };
        if overlap.base() != cursor {
            return Err(AddressSpaceError::NotMapped);
        }
        cursor = overlap.end();
    }
    if cursor != range.end() {
        return Err(AddressSpaceError::NotMapped);
    }
    Ok(())
}

fn reserve_mapping_ids<Backend: PageBackend, Account: MemoryAccount>(
    state: &mut AddressSpaceState<Backend, Account>,
    count: usize,
) -> Result<u64, AddressSpaceError<Backend::Error, Account::Error>> {
    let count = u64::try_from(count).map_err(|_| AddressSpaceError::SizeOverflow)?;
    let first = state.next_mapping_id;
    let next = first
        .checked_add(count)
        .ok_or(AddressSpaceError::IdentityExhausted)?;
    state.next_mapping_id = next;
    Ok(first)
}

fn validate_mapping_permissions<Backend: PageBackend, Account: MemoryAccount>(
    object: &MappingObject<Backend, Account>,
    permissions: Permissions,
) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
    if !permissions.is_valid() {
        return Err(AddressSpaceError::InvalidPermissions);
    }
    if matches!(object, MappingObject::Snapshot(_)) && permissions.contains(Access::Write) {
        return Err(AddressSpaceError::WriteDenied);
    }
    if object.executable() {
        if permissions.contains(Access::Write) {
            return Err(AddressSpaceError::WritableExecutableBacking);
        }
    } else if permissions.contains(Access::Execute) {
        return Err(AddressSpaceError::InvalidPermissions);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_fragments<Backend: PageBackend, Account: MemoryAccount>(
    space: &UserAddressSpace<Backend, Account>,
    output: &mut MappingIndex<Backend, Account>,
    original: &Mapping<Backend, Account>,
    cut: UserSlice,
    permissions: Option<Permissions>,
    generation: u64,
    next_token: &mut u64,
) -> Result<(), AddressSpaceError<Backend::Error, Account::Error>> {
    let overlap =
        intersection(original.snapshot.range, cut).ok_or(AddressSpaceError::InvalidRange)?;
    let fragments = [
        (original.snapshot.range.base(), overlap.base(), None),
        (overlap.base(), overlap.end(), permissions),
        (overlap.end(), original.snapshot.range.end(), None),
    ];
    let mut first = true;
    for (base, end, changed_permissions) in fragments {
        if base >= end || (base == overlap.base() && changed_permissions.is_none()) {
            continue;
        }
        let ownership = if first {
            first = false;
            original.ownership.clone()
        } else {
            space.try_mapping_ownership()?
        };
        let length = end.get() - base.get();
        let range = UserSlice::new(base, length).map_err(|_| AddressSpaceError::SizeOverflow)?;
        let delta = base.get() - original.snapshot.range.base().get();
        let id = *next_token;
        *next_token = next_token
            .checked_add(1)
            .ok_or(AddressSpaceError::IdentityExhausted)?;
        let fragment_permissions = match changed_permissions {
            Some(permissions) => permissions,
            None => original.snapshot.permissions,
        };
        *output = output.insert(
            Mapping {
                owner_vmar: original.owner_vmar,
                snapshot: MappingSnapshot {
                    token: MappingToken {
                        address_space: space.id,
                        base,
                        id,
                        generation,
                    },
                    range,
                    object_offset: original
                        .snapshot
                        .object_offset
                        .checked_add(delta)
                        .ok_or(AddressSpaceError::SizeOverflow)?,
                    permissions: fragment_permissions,
                    executable_backing: original.snapshot.executable_backing,
                },
                maximum_permissions: original.maximum_permissions,
                object: original.object.clone(),
                locks: original
                    .locks
                    .as_ref()
                    .map(|locks| locks.retain::<Backend>(range))
                    .transpose()?
                    .flatten(),
                ownership,
                write_lease: if fragment_permissions.contains(Access::Write)
                    && original.object.private().is_none()
                {
                    match &original.write_lease {
                        Some(lease) => Some(lease.clone()),
                        None => Some(original.object.try_write_lease()?),
                    }
                } else {
                    None
                },
            },
            &space.account,
        )?;
    }
    Ok(())
}

fn require_nonempty_aligned<BackendError, AccountError>(
    range: UserSlice,
) -> Result<(), AddressSpaceError<BackendError, AccountError>> {
    if range.length() == 0 {
        return Err(AddressSpaceError::EmptyRange);
    }
    require_page_aligned(range.base().get())?;
    require_page_aligned(range.length())
}

fn require_page_aligned<BackendError, AccountError>(
    value: u64,
) -> Result<(), AddressSpaceError<BackendError, AccountError>> {
    if !value.is_multiple_of(PAGE_SIZE) {
        return Err(AddressSpaceError::InvalidRange);
    }
    Ok(())
}

fn ranges_overlap(left: UserSlice, right: UserSlice) -> bool {
    left.base() < right.end() && right.base() < left.end()
}

fn intersection(left: UserSlice, right: UserSlice) -> Option<UserSlice> {
    let base = left.base().max(right.base());
    let end = left.end().min(right.end());
    if base >= end {
        return None;
    }
    UserSlice::new(base, end.get() - base.get()).ok()
}

fn fragment_total(mapping: UserSlice, cut: UserSlice, retain_middle: bool) -> usize {
    let Some(overlap) = intersection(mapping, cut) else {
        return 1;
    };
    usize::from(mapping.base() < overlap.base())
        + usize::from(retain_middle)
        + usize::from(overlap.end() < mapping.end())
}

/// Unmapping removes ownership of private pages which no remaining fragment
/// covers. The prepared version is unpublished; the old version still owns
/// every removed page until acknowledged hardware retirement.
fn prune_private_views<'a, B: PageBackend + 'a, A: MemoryAccount + 'a>(
    mappings: &mut MappingIndex<B, A>,
    affected: impl Iterator<Item = &'a Mapping<B, A>>,
    account: &A,
) -> SpaceResult<B, A, ()> {
    for mapping in affected {
        let Some(old) = mapping.object.private().cloned() else {
            continue;
        };
        // A prior affected fragment may already have replaced this view, or
        // the unmap may have removed every alias. Neither needs another copy.
        if !mappings.iter().any(|mapping| {
            mapping
                .object
                .private()
                .is_some_and(|view| core::ptr::eq(&**view, &*old))
        }) {
            continue;
        }
        let changed = old.retain_pages(|page| {
            let offset = page as u64 * PAGE_SIZE;
            mappings.iter().any(|mapping| {
                mapping
                    .object
                    .private()
                    .is_some_and(|view| core::ptr::eq(&**view, &*old))
                    && offset >= mapping.snapshot.object_offset
                    && offset < mapping.snapshot.object_offset + mapping.snapshot.range.length()
            })
        })?;
        let Some(changed) = changed else { continue };
        mappings.replace_private_view(&old, &changed, account)?;
    }
    Ok(())
}

/// Retains the writable lease and backing independently of mapping replacement.
pub(crate) struct PinnedAtomicWord<Backend: PageBackend, Account: MemoryAccount> {
    pub(crate) token: MappingToken,
    pub(crate) physical: u64,
    _backing: PinnedWordBacking<Backend, Account>,
    _charge: Account::Charge,
}

enum PinnedWordBacking<B: PageBackend, A: MemoryAccount> {
    Shared {
        _mapping: Mapping<B, A>,
    },
    Private {
        _page: super::vmo::PinnedPrivatePage<B, A>,
    },
}
