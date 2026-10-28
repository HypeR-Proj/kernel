// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Handle-visible native virtual-memory capabilities.

use hyper::mm::FallibleArc;

use super::address_space::ApplicationVmar;
use super::vmo::ExclusiveHardwareWriteLease;
use super::{
    DomainAccount, ExecutableAuthority, ExecutableVmo, KernelPageBackend, KernelPageError,
    NativeAddressSpace, SnapshotVmo, UserSlice, Vmar, VmoError, WritableVmo,
};
use crate::authority::Rights;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::ObjectPublication, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

type NativeWritableVmo = WritableVmo<KernelPageBackend, DomainAccount>;
type NativeSnapshotVmo = SnapshotVmo<KernelPageBackend, DomainAccount>;

type NativeExecutableVmo = ExecutableVmo<KernelPageBackend, DomainAccount>;

/// Stable writable VMO backing shared with a hardware address space.
///
/// This owner is intentionally independent of userspace handles. Retaining it
/// keeps every committed page stable after the source handle closes, while the
/// exclusive hardware lease prevents direct writers, writable Native mappings,
/// or snapshot publication from racing hardware writes.
#[derive(Clone)]
pub(crate) struct GuestMemoryBacking {
    storage: NativeWritableVmo,
    _mapping: ExclusiveHardwareWriteLease<KernelPageBackend, DomainAccount>,
}

impl GuestMemoryBacking {
    pub(crate) fn try_from_vmo(vmo: &VmoObject) -> Result<Self, MemoryObjectError> {
        let storage = vmo
            .writable_clone()
            .ok_or(MemoryObjectError::WrongVariant)?;
        let mapping = storage.try_exclusive_hardware_write_lease()?;
        Ok(Self {
            storage,
            _mapping: mapping,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.storage.size()
    }

    pub(crate) fn same_storage(&self, other: &Self) -> bool {
        self.storage.same_storage(&other.storage)
    }

    pub(crate) fn resident_bytes(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<u64, MemoryObjectError> {
        self.storage
            .resident_bytes(offset, length)
            .map_err(MemoryObjectError::Vmo)
    }

    pub(crate) fn populate_page(&self, offset: u64) -> Result<(), MemoryObjectError> {
        self.storage
            .populate(offset, hyper::mm::PAGE_SIZE)
            .map_err(|failure| MemoryObjectError::Vmo(failure.cause))
    }

    pub(crate) fn physical_page(
        &self,
        offset: u64,
    ) -> Result<hyper::mm::PhysicalAddress, MemoryObjectError> {
        self.storage
            .resident_physical_page(offset)
            .map_err(MemoryObjectError::Vmo)
    }

    pub(crate) fn read_exposed(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), MemoryObjectError> {
        self.storage
            .read_exposed(offset, destination)
            .map_err(MemoryObjectError::Vmo)
    }

    pub(crate) fn write_exposed(
        &self,
        offset: u64,
        source: &[u8],
    ) -> Result<(), MemoryObjectError> {
        self.storage
            .write_exposed(offset, source)
            .map_err(MemoryObjectError::Vmo)
    }

    /// Copies Native-owned request pages into the dedicated guest exchange
    /// storage without exposing either side as a Rust byte slice.
    pub(crate) fn copy_from_native(
        &self,
        source: &NativeWritableVmo,
        source_offset: u64,
        destination_offset: u64,
        length: usize,
    ) -> Result<(), MemoryObjectError> {
        self.storage
            .copy_from_exposed_vmo(source, source_offset, destination_offset, length)
            .map_err(MemoryObjectError::Vmo)
    }

    pub(crate) fn page_is_resident(&self, offset: u64) -> Result<bool, MemoryObjectError> {
        self.storage
            .page_is_resident(offset)
            .map_err(MemoryObjectError::Vmo)
    }
}

/// Failure while preparing an accounted virtual-memory capability object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryObjectError {
    AlreadyPublished,
    AllocationSize,
    Object(ObjectCreationError),
    Resource(ResourceError),
    AddressSpace(super::AddressSpaceError<super::KernelPageError, ResourceError>),
    Vmo(VmoError<KernelPageError, ResourceError>),
    WrongVariant,
}

impl From<ObjectCreationError> for MemoryObjectError {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<ResourceError> for MemoryObjectError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<VmoError<KernelPageError, ResourceError>> for MemoryObjectError {
    fn from(error: VmoError<KernelPageError, ResourceError>) -> Self {
        Self::Vmo(error)
    }
}

enum VmoStorage {
    Snapshot(NativeSnapshotVmo),
    Writable(NativeWritableVmo),
    ReadOnly(NativeWritableVmo),
    Executable(NativeExecutableVmo),
}

/// One handle-visible VMO whose immutable variant fixes its authority ceiling.
///
/// Writable and executable backing share an ABI kind so a mapping operation
/// can accept either. Their per-instance rights differ: no executable snapshot
/// can acquire `WRITE`, and writable storage cannot acquire
/// `EXECUTE` merely because both variants use this Rust payload type.
pub(crate) struct VmoObject {
    storage: VmoStorage,
    _object_charge: CommittedCharge,
}

impl VmoObject {
    pub(crate) fn try_new_contiguous(
        size: u64,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        if size < hyper::mm::PAGE_SIZE
            || !size.is_power_of_two()
            || size > hyper::abi::native::HYPER_NATIVE_VMO_MAX_CONTIGUOUS_SIZE_BYTES
        {
            return Err(MemoryObjectError::AllocationSize);
        }
        let layout = NativeWritableVmo::metadata_layout(size)?;
        let pages = crate::mm::cache_memory::allocation_page_bound(layout)
            .ok_or(MemoryObjectError::AllocationSize)?;
        crate::mm::reclaim::retry_prepare(
            crate::mm::reclaim::Target::PhysicalOrder(pages.trailing_zeros() as usize),
            || {
                let (pages, transient) = super::kernel_adapter::contiguous_pages(size, sponsor)?;
                let storage = WritableVmo::try_from_owned_pages(
                    size,
                    KernelPageBackend,
                    DomainAccount::new(sponsor.clone()),
                    pages,
                )?;
                drop(transient);
                Self::from_writable(storage, sponsor)
            },
            |error| matches!(error, MemoryObjectError::Vmo(VmoError::Allocation)),
        )
    }

    pub(crate) fn try_new_writable(
        size: u64,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        let layout = NativeWritableVmo::metadata_layout(size)?;
        let pages = crate::mm::cache_memory::allocation_page_bound(layout)
            .ok_or(MemoryObjectError::AllocationSize)?;
        let storage = crate::mm::reclaim::retry_prepare(
            crate::mm::reclaim::Target::PhysicalOrder(pages.trailing_zeros() as usize),
            || WritableVmo::try_new(size, KernelPageBackend, DomainAccount::new(sponsor.clone())),
            |error| matches!(error, VmoError::Allocation),
        )?;
        Self::from_writable(storage, sponsor)
    }

    fn from_writable(
        storage: NativeWritableVmo,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        Ok(Self {
            storage: VmoStorage::Writable(storage),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    /// Publishes only observation of Native-owned mutable storage. Unlike an
    /// attenuated writable handle, this object cannot ever support WRITE or
    /// guest hardware ownership, even if a publisher requests broader rights.
    pub(crate) fn from_read_only_native(
        storage: NativeWritableVmo,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        Ok(Self {
            storage: VmoStorage::ReadOnly(storage),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn read_only_clone(&self) -> Option<NativeWritableVmo> {
        match &self.storage {
            VmoStorage::ReadOnly(storage) => Some(storage.clone()),
            _ => None,
        }
    }

    pub(crate) fn from_executable(
        storage: NativeExecutableVmo,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        Ok(Self {
            storage: VmoStorage::Executable(storage),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn from_snapshot(
        storage: NativeSnapshotVmo,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        Ok(Self {
            storage: VmoStorage::Snapshot(storage),
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    /// Returns immutable bytes without granting executable or shared-write authority.
    pub(crate) fn snapshot_clone(&self) -> Option<NativeSnapshotVmo> {
        match &self.storage {
            VmoStorage::Snapshot(storage) => Some(storage.clone()),
            VmoStorage::Executable(storage) => Some(storage.snapshot()),
            VmoStorage::Writable(_) | VmoStorage::ReadOnly(_) => None,
        }
    }

    pub(crate) fn try_snapshot(&self, sponsor: &ResourceDomain) -> Result<Self, MemoryObjectError> {
        let snapshot = match &self.storage {
            VmoStorage::Writable(storage) | VmoStorage::ReadOnly(storage) => {
                storage.try_snapshot(DomainAccount::new(sponsor.clone()))?
            }
            VmoStorage::Snapshot(storage) => storage.clone(),
            VmoStorage::Executable(storage) => storage.snapshot(),
        };
        Self::from_snapshot(snapshot, sponsor)
    }

    pub(crate) fn writable(&self) -> Option<&NativeWritableVmo> {
        match &self.storage {
            VmoStorage::Writable(storage) => Some(storage),
            VmoStorage::Executable(_) | VmoStorage::Snapshot(_) | VmoStorage::ReadOnly(_) => None,
        }
    }

    pub(crate) fn writable_clone(&self) -> Option<NativeWritableVmo> {
        self.writable().cloned()
    }

    pub(crate) fn executable(&self) -> Option<&NativeExecutableVmo> {
        match &self.storage {
            VmoStorage::Writable(_) | VmoStorage::Snapshot(_) | VmoStorage::ReadOnly(_) => None,
            VmoStorage::Executable(storage) => Some(storage),
        }
    }

    pub(crate) fn executable_clone(&self) -> Option<NativeExecutableVmo> {
        self.executable().cloned()
    }

    pub(crate) fn size(&self) -> u64 {
        match &self.storage {
            VmoStorage::Writable(storage) | VmoStorage::ReadOnly(storage) => storage.size(),
            VmoStorage::Snapshot(storage) => storage.size(),
            VmoStorage::Executable(storage) => storage.size(),
        }
    }

    pub(crate) fn read(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), MemoryObjectError> {
        match &self.storage {
            VmoStorage::Writable(storage) => storage.read(offset, destination)?,
            VmoStorage::ReadOnly(storage) => storage.read_exposed(offset, destination)?,
            VmoStorage::Snapshot(storage) => storage.read(offset, destination)?,
            VmoStorage::Executable(storage) => storage.read(offset, destination)?,
        }
        Ok(())
    }

    pub(crate) fn write(&self, offset: u64, source: &[u8]) -> Result<(), MemoryObjectError> {
        let storage = self.writable().ok_or(MemoryObjectError::WrongVariant)?;
        storage.write(offset, source)?;
        Ok(())
    }

    /// Derives a physically distinct immutable executable snapshot.
    ///
    /// The caller must have independently resolved both the writable VMO with
    /// `READ` and the authority object with `CREATE_EXECUTABLE`. The pinned
    /// execution proof supplies the architecture cache-publication context.
    #[expect(
        dead_code,
        reason = "reserved executable-authority conversion has no Native syscall consumer yet"
    )]
    pub(crate) fn try_executable_snapshot<P: hyper::cpu::PinnedExecution + 'static>(
        &self,
        authority: &ExecutableAuthority,
        pin: &P,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        let writable = self.writable().ok_or(MemoryObjectError::WrongVariant)?;
        let executable = writable.try_executable_snapshot(&authority.provenance(), pin)?;
        Self::from_executable(executable, sponsor)
    }
}

impl private::Sealed for VmoObject {}
impl private::UserExportable for VmoObject {}

impl KernelObject for VmoObject {
    const KIND: ObjectKind = ObjectKind::VMO;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::MAP)
        .union(Rights::EXECUTE);

    fn supported_rights(&self) -> Rights {
        let common = Rights::DUPLICATE
            .union(Rights::TRANSFER)
            .union(Rights::INSPECT)
            .union(Rights::READ)
            .union(Rights::MAP);
        match &self.storage {
            VmoStorage::Writable(_) => common.union(Rights::WRITE),
            VmoStorage::Snapshot(_) | VmoStorage::ReadOnly(_) => common,
            VmoStorage::Executable(_) => common.union(Rights::EXECUTE),
        }
    }
}

/// Handle-visible authority over one VMAR in a native address space.
///
/// The strong address-space owner keeps the token's generation namespace
/// alive. Mapping policy still validates the token on every operation, so a
/// destroyed child VMAR remains stale even while this object is referenced.
pub(crate) struct VmarObject {
    address_space: FallibleArc<NativeAddressSpace>,
    token: ApplicationVmar,
    _object_charge: CommittedCharge,
}

impl VmarObject {
    /// Initial authority installed for a Process's own root VMAR.
    pub(crate) const ROOT_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::MAP);

    fn root(
        address_space: FallibleArc<NativeAddressSpace>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        let token = address_space
            .logical()
            .application_vmar(address_space.logical().root_vmar())
            .ok_or(MemoryObjectError::AddressSpace(
                super::AddressSpaceError::InvalidRange,
            ))?;
        Ok(Self {
            address_space,
            token,
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    /// Wrap an existing loader-created VMAR without allocating another region.
    /// Closing this handle does not destroy its address-space-owned reservation.
    pub(crate) fn try_existing(
        address_space: FallibleArc<NativeAddressSpace>,
        token: Vmar,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        let token = address_space
            .logical()
            .application_vmar(token)
            .ok_or(MemoryObjectError::AllocationSize)?;
        Ok(Self {
            address_space,
            token,
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_child(
        parent: &Self,
        address: u64,
        size: u64,
        exact: bool,
        sponsor: &ResourceDomain,
    ) -> Result<Self, MemoryObjectError> {
        let object_charge = reserve_object_charge::<Self>(sponsor)?;
        let space = parent.address_space.logical();
        let token = if !exact {
            space.try_create_vmar_hint(parent.token.token(), size, address)
        } else {
            let range = UserSlice::new(super::UserAddress::new(address), size)
                .map_err(|_| MemoryObjectError::AllocationSize)?;
            space.try_create_vmar(parent.token.token(), range)
        }
        .map_err(MemoryObjectError::AddressSpace)?;
        // Child construction proves containment within the checked parent.
        let token = parent
            .address_space
            .logical()
            .application_vmar(token)
            .ok_or(MemoryObjectError::AddressSpace(
                super::AddressSpaceError::InvalidRange,
            ))?;
        Ok(Self {
            address_space: parent.address_space.clone(),
            token,
            _object_charge: object_charge,
        })
    }

    /// Constructs the single handle-visible identity for the root VMAR.
    pub(crate) fn try_root_publication(
        address_space: FallibleArc<NativeAddressSpace>,
        sponsor: &ResourceDomain,
    ) -> Result<ObjectPublication<Self>, MemoryObjectError> {
        if !address_space.claim_root_vmar_object_publication() {
            return Err(MemoryObjectError::AlreadyPublished);
        }
        let result = Self::root(address_space.clone(), sponsor)
            .and_then(|payload| ObjectPublication::try_new(payload).map_err(Into::into));
        if result.is_err() {
            address_space.abort_root_vmar_object_publication();
        }
        result
    }

    /// Rolls back an unpublished root-object claim after authority preparation.
    pub(crate) fn abort_root_publication(address_space: &NativeAddressSpace) {
        address_space.abort_root_vmar_object_publication();
    }

    pub(crate) fn address_space(&self) -> &NativeAddressSpace {
        &self.address_space
    }

    pub(crate) const fn token(&self) -> Vmar {
        self.token.token()
    }
}

impl private::Sealed for VmarObject {}
impl private::UserExportable for VmarObject {}

impl KernelObject for VmarObject {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        use super::address_space::{VmarDetailError, VmarDetailRecord};
        use {
            crate::object::diagnostics::DetailError, crate::object::diagnostics::DetailRecord,
            crate::object::diagnostics::Details,
        };
        let page = self
            .address_space
            .logical()
            .vmar_details(self.token.token(), cursor)
            .map_err(|error| match error {
                VmarDetailError::InvalidCursor => DetailError::InvalidCursor,
                VmarDetailError::Stale => DetailError::Stale,
            })?;
        let bits = |permissions: super::contract::Permissions| {
            use super::contract::Access;
            u64::from(permissions.contains(Access::Read))
                | (u64::from(permissions.contains(Access::Write)) << 1)
                | (u64::from(permissions.contains(Access::Execute)) << 2)
        };
        let record = match page.record {
            VmarDetailRecord::Empty => DetailRecord::Empty,
            VmarDetailRecord::Region { range, live } => DetailRecord::Vmar {
                base: range.base().get(),
                length: range.length(),
                live,
            },
            VmarDetailRecord::Mapping {
                range,
                permissions,
                maximum_permissions,
            } => DetailRecord::Mapping {
                base: range.base().get(),
                length: range.length(),
                permissions: bits(permissions),
                maximum_permissions: bits(maximum_permissions),
            },
        };
        Ok(Details {
            record,
            next_cursor: page.next_cursor,
        })
    }

    const KIND: ObjectKind = ObjectKind::VMAR;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::MAP)
        .union(Rights::LOCK_MEMORY);
}

fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, MemoryObjectError> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(MemoryObjectError::AllocationSize)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}
