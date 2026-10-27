// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native machine-root ownership, admission, and acknowledged replacement.

use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hyper::cpu::PerCpu;
use hyper::mm::{AddressSpaceResidency, FallibleArc, PAGE_SIZE, ResidencyError, UniqueFallibleArc};
use hyper::sync::InterruptSpinLock;

use super::kernel_adapter::DomainCharge;
use super::{
    Access, AddressError, AddressSpaceError as LogicalError, DomainAccount, KernelPageBackend,
    KernelPageError, MappingChange, MemoryAccount, MemoryCharge, Permissions,
    PreparedMappingChange, PreparedUserWrite, UserAddress, UserAddressSpace, UserSlice, VmoError,
    WritableVmo,
};
use crate::mm::page_block::PageBlock;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::mm::translation_id::ActiveIdentifier, crate::mm::translation_id::HostAsid,
    crate::mm::translation_id::IdentifierLease,
};

pub(crate) type NativePinnedAtomicWord =
    super::address_space::PinnedAtomicWord<KernelPageBackend, DomainAccount>;

type LogicalAddressSpace = UserAddressSpace<KernelPageBackend, DomainAccount>;
pub(crate) type NativeCrashMemory = super::CrashMemory<KernelPageBackend, DomainAccount>;
type LogicalPrepared<'a> = PreparedMappingChange<'a, KernelPageBackend, DomainAccount>;
type LogicalUserWrite = PreparedUserWrite<KernelPageBackend, DomainAccount>;
type StateLock = InterruptSpinLock<MachineState, crate::hal::irq::LocalMask>;
type LogicalAddressSpaceError = LogicalError<KernelPageError, ResourceError>;

static ACTIVE_OWNER: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_EPOCH: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE: PerCpu<AtomicBool> =
    PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);

#[derive(Debug)]
pub(crate) enum Error {
    Allocation,
    Address(AddressError),
    #[expect(dead_code, reason = "error payload is retained for Debug diagnostics")]
    Hal(crate::hal::user::AddressSpaceError),
    Identifier(crate::mm::translation_id::Error),
    Logical(LogicalAddressSpaceError),
    Page(crate::mm::allocator::BuddyError),
    Residency(ResidencyError),
    Resource(ResourceError),
    SizeOverflow,
    Transport,
    Unsupported,
    Vmo(VmoError<KernelPageError, ResourceError>),
}

impl Error {
    pub(crate) const fn is_identifier_busy(&self) -> bool {
        matches!(
            self,
            Self::Identifier(crate::mm::translation_id::Error::Busy)
        )
    }

    pub(crate) const fn is_stale_mapping_transaction(&self) -> bool {
        match self {
            Self::Logical(error) => error.is_stale_transaction(),
            Self::Residency(ResidencyError::StaleEpoch) => true,
            _ => false,
        }
    }

    pub(crate) const fn is_mapping_conflict(&self) -> bool {
        matches!(
            self,
            Self::Logical(
                LogicalError::Busy
                    | LogicalError::StaleTransaction
                    | LogicalError::CowRequired
                    | LogicalError::BackingNotResident
            ) | Self::Residency(ResidencyError::Busy | ResidencyError::StaleEpoch)
        )
    }
}

#[must_use = "recover and retry the exact native address-space owner"]
pub(crate) struct RetirementFailure {
    error: Error,
    owner: UniqueFallibleArc<NativeAddressSpace>,
}

impl RetirementFailure {
    pub(crate) fn into_parts(self) -> (Error, UniqueFallibleArc<NativeAddressSpace>) {
        (self.error, self.owner)
    }
}

impl From<crate::hal::user::AddressSpaceError> for Error {
    fn from(error: crate::hal::user::AddressSpaceError) -> Self {
        Self::Hal(error)
    }
}

impl From<crate::mm::translation_id::Error> for Error {
    fn from(error: crate::mm::translation_id::Error) -> Self {
        match error {
            crate::mm::translation_id::Error::Allocation => Self::Allocation,
            other => Self::Identifier(other),
        }
    }
}

impl From<LogicalAddressSpaceError> for Error {
    fn from(error: LogicalAddressSpaceError) -> Self {
        Self::Logical(error)
    }
}

impl From<AddressError> for Error {
    fn from(error: AddressError) -> Self {
        Self::Address(error)
    }
}

impl From<VmoError<KernelPageError, ResourceError>> for Error {
    fn from(error: VmoError<KernelPageError, ResourceError>) -> Self {
        Self::Vmo(error)
    }
}

impl From<ResidencyError> for Error {
    fn from(error: ResidencyError) -> Self {
        Self::Residency(error)
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

mod tables;
use tables::TablePagePool;

struct MachineImage {
    epoch: u64,
    root: crate::hal::user::PreparedAddressSpace,
    _tables: TablePagePool,
    _object_charge: DomainCharge,
}

struct MachineState {
    current: FallibleArc<MachineImage>,
    residency: AddressSpaceResidency<{ hyper::cpu::MAX_CPUS }>,
}

/// Logical and machine state for one native userspace address space.
///
/// Safe Drop deliberately leaks the published root and identifier. A future
/// Process teardown path will consume an acknowledged final-retirement token;
/// freeing either owner without it would permit hardware use-after-free.
pub(crate) struct NativeAddressSpace {
    logical: ManuallyDrop<LogicalAddressSpace>,
    account: DomainAccount,
    identifier: ManuallyDrop<ActiveIdentifier<HostAsid>>,
    state: ManuallyDrop<StateLock>,
    root_vmar_object_published: AtomicBool,
    _owner_charge: CommittedCharge,
}

/// Unpublished, independently owned loader segments. No installed segment
/// retains the temporary whole-file parser snapshot.
pub(crate) struct NativeImageSegment {
    range: UserSlice,
    permissions: Permissions,
    storage: ImageStorage,
}

enum ImageStorage {
    Writable(WritableVmo<KernelPageBackend, DomainAccount>),
    Private(super::vmo::PrivateViewBuilder<KernelPageBackend, DomainAccount>),
}

impl NativeImageSegment {
    pub(crate) fn try_new(
        address_space: &NativeAddressSpace,
        range: UserSlice,
        permissions: Permissions,
    ) -> Result<Self, Error> {
        if !permissions.is_valid() || permissions == Permissions::NONE {
            return Err(Error::Unsupported);
        }
        let storage = WritableVmo::try_new(
            range.length(),
            KernelPageBackend,
            address_space.account.clone(),
        )?;
        storage
            .populate(0, range.length())
            .map_err(|failure| Error::Vmo(failure.cause))?;
        Ok(Self {
            range,
            permissions,
            storage: ImageStorage::Writable(storage),
        })
    }

    pub(crate) fn try_from_bytes(
        address_space: &NativeAddressSpace,
        range: UserSlice,
        permissions: Permissions,
        bytes: &[u8],
        data_offset: u64,
    ) -> Result<Self, Error> {
        if !permissions.is_valid() || permissions == Permissions::NONE {
            return Err(Error::Unsupported);
        }
        // Copy only this LOAD segment. The source parser buffer can be released
        // immediately after all segments have been installed.
        let file_bytes = if bytes.is_empty() {
            0
        } else {
            (bytes.len() as u64)
                .checked_sub(data_offset)
                .ok_or(Error::Unsupported)?
        };
        let source = super::SnapshotVmo::try_from_bytes(
            bytes,
            KernelPageBackend,
            address_space.account.clone(),
        )?;
        let view = super::vmo::PrivateViewBuilder::try_new(
            source,
            range.length(),
            0,
            file_bytes,
            data_offset,
            super::PrivateMappingMode::CopyOnWrite,
            KernelPageBackend,
            address_space.account.clone(),
        )?;
        Ok(Self {
            range,
            permissions,
            storage: ImageStorage::Private(view),
        })
    }

    pub(crate) fn install(self, address_space: &NativeAddressSpace) -> Result<(), Error> {
        self.install_in_vmar(address_space, address_space.logical().root_vmar())
    }

    pub(crate) fn install_in_vmar(
        self,
        address_space: &NativeAddressSpace,
        vmar: super::Vmar,
    ) -> Result<(), Error> {
        let logical = address_space.logical();
        let prepared = match self.storage {
            ImageStorage::Writable(storage) => logical.prepare_map_writable(
                vmar,
                self.range,
                storage,
                0,
                self.permissions,
                self.permissions,
            )?,
            ImageStorage::Private(storage) => {
                let storage = if self.permissions.contains(Access::Execute) {
                    // Instruction publication requires a stable CPU, but mapping
                    // commit may yield while another loader owns the cross-call
                    // transport. Release this pin before preparing that commit,
                    // including when executable publication fails.
                    let pin =
                        crate::task::scheduler::preempt_disable().map_err(|_| Error::Transport)?;
                    let published = storage.finish_executable(
                        &super::ExecutableProvenance::for_native_image_loader(),
                        &pin,
                    );
                    crate::task::scheduler::preempt_enable_without_reschedule(pin)
                        .map_err(|_| Error::Transport)?;
                    published?
                } else {
                    storage.finish()
                };
                logical.prepare_map_private_view(
                    vmar,
                    self.range,
                    storage,
                    self.permissions,
                    self.permissions,
                )?
            }
        };
        address_space.prepare_change(prepared)?.commit()?;
        Ok(())
    }
}

impl NativeAddressSpace {
    pub(crate) fn try_new(
        domain: ResourceDomain,
        range: UserSlice,
    ) -> Result<UniqueFallibleArc<Self>, Error> {
        let plan = crate::hal::user::address_space_plan()?;
        let owner_bytes = u64::try_from(UniqueFallibleArc::<Self>::allocation_size())
            .map_err(|_| Error::SizeOverflow)?;
        let owner_charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, owner_bytes),
            )?
            .commit();
        // Reserve the final pinned owner before publishing any root or
        // identifier, so later allocation failure cannot strand hardware.
        let owner: UniqueFallibleArc<core::mem::MaybeUninit<Self>> =
            UniqueFallibleArc::try_new_uninit().map_err(|_| Error::Allocation)?;
        let window = super::address_window(&plan)?;
        let account = DomainAccount::new(domain);
        let logical = UserAddressSpace::try_new(window, range, KernelPageBackend, account.clone())?;
        let reserved = crate::mm::translation_id::reserve::<HostAsid>(plan.asid_bits())?;
        let initial_epoch = logical.mapping_epoch();
        let image = empty_image(reserved.generation(), initial_epoch, account.clone())?;
        let residency = AddressSpaceResidency::try_new(initial_epoch)?;
        let identifier = reserved.activate()?;
        Ok(owner.write(Self {
            logical: ManuallyDrop::new(logical),
            account,
            identifier: ManuallyDrop::new(identifier),
            state: ManuallyDrop::new(InterruptSpinLock::new(MachineState {
                current: image,
                residency,
            })),
            root_vmar_object_published: AtomicBool::new(false),
            _owner_charge: owner_charge,
        }))
    }

    /// Reserve a child range before installing any loader-owned stack pages.
    /// Failure of the unpublished image retires this reservation with its root.
    pub(crate) fn reserve_initial_stack(&self, range: UserSlice) -> Result<super::Vmar, Error> {
        Ok(self
            .logical()
            .try_create_vmar(self.logical().root_vmar(), range)?)
    }

    pub(super) fn logical(&self) -> &LogicalAddressSpace {
        &self.logical
    }

    pub(super) fn claim_root_vmar_object_publication(&self) -> bool {
        self.root_vmar_object_published
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn abort_root_vmar_object_publication(&self) {
        if self
            .root_vmar_object_published
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            hyper::debug::invariant_failure(
                "mm::user_space::machine::abort_root_vmar_object_publication invariant",
            );
        }
    }

    /// Prepares private backing before any kernel or userspace write can use it.
    /// The immutable old mapping set keeps shared pages alive through every
    /// active-root replacement and inactive-resident invalidation acknowledgement.
    #[inline(never)]
    pub(crate) fn resolve_private_write(&self, destination: UserSlice) -> Result<(), Error> {
        self.populate_file_range(destination)?;
        if let Some(prepared) = self.prepare_private_write_change(destination)? {
            self.prepare_change(prepared)?.commit()?;
        }
        Ok(())
    }

    // Mapping traversal needs temporary stack space. Release that frame before
    // constructing page tables or yielding to acquire the shootdown transaction.
    // The returned logical owner retains every prepared backing resource.
    #[inline(never)]
    fn prepare_private_write_change(
        &self,
        destination: UserSlice,
    ) -> Result<Option<LogicalPrepared<'_>>, Error> {
        Ok(self.logical.prepare_private_write(destination)?)
    }

    /// Resolves only an admitted private mapping, including a stale fault after
    /// a sibling Thread has already materialized that page. Ordinary protection
    /// violations must never be converted into an endless instruction retry.
    pub(crate) fn resolve_write_fault(
        &self,
        address: UserAddress,
        observed_epoch: u64,
    ) -> Result<bool, Error> {
        if !self.logical.is_private_write(address) {
            return Ok(false);
        }
        let page = UserAddress::new(address.get() / PAGE_SIZE * PAGE_SIZE);
        self.resolve_private_write(UserSlice::new(page, PAGE_SIZE)?)?;
        Ok(self.logical.mapping_epoch() != observed_epoch)
    }

    /// Only stable file identity and range metadata survive clean-page eviction.
    #[inline(never)]
    pub(crate) fn populate_file_range(&self, range: UserSlice) -> Result<(), Error> {
        if let Some(prepared) = self.prepare_file_population_change(range)? {
            self.prepare_change(prepared)?.commit()?;
        }
        Ok(())
    }

    // Keep file-population scratch out of the nested COW and commit callers.
    #[inline(never)]
    fn prepare_file_population_change(
        &self,
        range: UserSlice,
    ) -> Result<Option<LogicalPrepared<'_>>, Error> {
        Ok(self.logical.prepare_file_population(range)?)
    }

    pub(crate) fn resolve_file_fault(
        &self,
        address: UserAddress,
        access: Access,
        observed_epoch: u64,
    ) -> Result<bool, Error> {
        if !self.logical.is_file_access(address, access) {
            return Ok(false);
        }
        let page = UserAddress::new(address.get() / PAGE_SIZE * PAGE_SIZE);
        let range = UserSlice::new(page, PAGE_SIZE)?;
        self.populate_file_range(range)?;
        if access == Access::Write {
            self.resolve_private_write(range)?;
        }
        Ok(self.logical.mapping_epoch() != observed_epoch)
    }

    pub(crate) fn next_file_reclaim_range(
        &self,
        after_address: u64,
        limit: usize,
    ) -> Option<UserSlice> {
        self.logical.next_file_reclaim_range(after_address, limit)
    }

    /// A successful replacement acknowledges every resident CPU before old
    /// physical pages are dropped. Dirty/COW pages are excluded by the logical
    /// plan, and a concurrent write or unmap invalidates that plan's epoch.
    pub(crate) fn reclaim_file_range(&self, range: UserSlice) -> Result<usize, Error> {
        let Some((prepared, count)) = self.logical.prepare_file_reclaim(range)? else {
            return Ok(0);
        };
        self.prepare_change(prepared)?.commit()?;
        Ok(count)
    }

    /// Only the owning process may call this, after all its threads and active
    /// handles have retired. No allocation/quota headroom is needed to release
    /// content protection, even while foreign VMAR references retain this owner.
    pub(crate) fn retire_file_sources(&self) {
        self.logical.retire_file_sources();
    }

    pub(crate) fn begin_process_stop(&self) {
        self.logical.begin_process_stop();
    }

    /// Retains bounded resident private pages without retaining this space.
    ///
    /// # Safety
    /// Every target user Thread and in-flight syscall must be stopped, its
    /// active handles withdrawn, and no hardware execution may resume. Calling
    /// `begin_process_stop` first seals foreign VMAR mutation and copy admission.
    pub(crate) unsafe fn capture_crash_memory(
        &self,
        max_bytes: u64,
    ) -> Result<NativeCrashMemory, Error> {
        self.logical
            .capture_crash_memory(max_bytes)
            .map_err(Error::Logical)
    }

    /// Copies kernel-owned bytes through the logical user-address contract.
    pub(crate) fn copy_to_user(&self, destination: UserSlice, source: &[u8]) -> Result<(), Error> {
        match self.logical.copy_to_user(destination, source) {
            Ok(()) => Ok(()),
            Err(LogicalError::CowRequired | LogicalError::BackingNotResident) => {
                self.resolve_private_write(destination)?;
                self.logical
                    .copy_to_user(destination, source)
                    .map_err(Error::Logical)
            }
            Err(error) => Err(Error::Logical(error)),
        }
    }

    /// Copies bytes from the current logical user-address mappings.
    pub(crate) fn copy_from_user(
        &self,
        source: UserSlice,
        destination: &mut [u8],
    ) -> Result<(), Error> {
        match self.logical.copy_from_user(source, destination) {
            Ok(()) => Ok(()),
            Err(LogicalError::BackingNotResident) => {
                self.populate_file_range(source)?;
                self.logical
                    .copy_from_user(source, destination)
                    .map_err(Error::Logical)
            }
            Err(error) => Err(Error::Logical(error)),
        }
    }

    pub(crate) fn atomic_wait_key(
        &self,
        address: UserAddress,
    ) -> Result<super::MappingToken, Error> {
        self.logical
            .atomic_wait_key(address)
            .map_err(Error::Logical)
    }

    pub(crate) fn pin_atomic_u32(
        &self,
        address: UserAddress,
    ) -> Result<NativePinnedAtomicWord, Error> {
        match self.logical.pin_atomic_u32(address) {
            Ok(pin) => Ok(pin),
            Err(LogicalError::CowRequired | LogicalError::BackingNotResident) => {
                self.resolve_private_write(UserSlice::new(address, 4)?)?;
                self.logical.pin_atomic_u32(address).map_err(Error::Logical)
            }
            Err(error) => Err(Error::Logical(error)),
        }
    }

    /// Reserves a stable writable mapping for capability-returning syscalls.
    pub(crate) fn reserve_user_write(
        owner: FallibleArc<Self>,
        destination: UserSlice,
    ) -> Result<UserWriteReservation, Error> {
        let plan = match owner.logical.prepare_user_write(destination) {
            Ok(plan) => plan,
            Err(LogicalError::CowRequired | LogicalError::BackingNotResident) => {
                owner.resolve_private_write(destination)?;
                owner.logical.prepare_user_write(destination)?
            }
            Err(error) => return Err(Error::Logical(error)),
        };
        Ok(UserWriteReservation {
            owner,
            plan: Some(plan),
        })
    }

    pub(crate) fn prepare_change<'a>(
        &'a self,
        logical: LogicalPrepared<'a>,
    ) -> Result<PreparedNativeChange<'a>, Error> {
        let pin = crate::task::scheduler::preempt_disable().map_err(|_| Error::Transport)?;
        let published = logical.publish_file_instructions(&pin);
        crate::task::scheduler::preempt_enable_without_reschedule(pin)
            .map_err(|_| Error::Transport)?;
        published?;
        let previous = self.state.with(|state| {
            if state.current.epoch != logical.base_epoch() {
                return Err(Error::Residency(ResidencyError::StaleEpoch));
            }
            Ok(state.current.clone())
        })?;
        let mut tables = previous._tables.fork(logical.next_epoch());
        let mut logical_result = Ok(());
        // SAFETY: The previous image remains retained. The fork copies each
        // edited table before writing and owns shared untouched tables directly.
        // Logical snapshots retain every mapped data page through commit/abort.
        let root = unsafe {
            crate::hal::user::edit_host_address_space(
                &previous.root,
                |visit| {
                    logical_result = logical.visit_updates(visit);
                },
                &mut tables,
            )
        };
        logical_result?;
        let root = table_result(root, &mut tables)?;
        let image = finish_image(logical.next_epoch(), root, tables, self.account.clone())?;
        Ok(PreparedNativeChange {
            owner: self,
            logical,
            image,
        })
    }

    /// Acquire before local activation, so temporary tag pressure can retry
    /// without publishing a partially installed translation context.
    pub(crate) fn acquire_identifier(&self) -> Result<IdentifierLease<HostAsid>, Error> {
        self.identifier.acquire().map_err(Error::Identifier)
    }

    /// Installs this address space for the current Thread's execution interval.
    ///
    /// # Safety
    ///
    /// Interrupts must remain masked until the activation is registered with
    /// the Native scheduler switch hook. That hook must consume the token on
    /// this CPU before every actual context switch. No active token may remain
    /// in a suspended or migrating Thread's continuation.
    pub(crate) unsafe fn activate<'a>(
        &'a self,
        kernel_access: &crate::hal::user::PreparedKernelAccess<'_>,
        identifier: IdentifierLease<HostAsid>,
    ) -> Result<ActiveNativeAddressSpace<'a>, Error> {
        if identifier.generation() != self.identifier.generation() {
            return Err(Error::Identifier(
                crate::mm::translation_id::Error::InvalidToken,
            ));
        }
        let cpu = crate::cpu::current_index().ok_or(Error::Unsupported)?;
        let owner = self.logical.id().get();
        if ACTIVE_OWNER[cpu]
            .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Residency(ResidencyError::AlreadyActive));
        }
        let backend = self.state.with(|state| {
            state
                .residency
                .check_admission(cpu.get(), state.current.epoch)?;
            // SAFETY: The state lock closes the update cut through the local
            // install and active publication. The caller keeps IRQs masked
            // until the scheduler hook owns this activation.
            let backend = unsafe {
                crate::hal::user::activate_local(
                    &state.current.root,
                    identifier.value(),
                    identifier.epoch(),
                    cpu,
                    self,
                    kernel_access,
                )?
            };
            ACTIVE_EPOCH[cpu].store(state.current.epoch, Ordering::Relaxed);
            ACTIVE[cpu].store(true, Ordering::Release);
            if let Err(error) = state.residency.publish_admission(cpu.get()) {
                ACTIVE[cpu].store(false, Ordering::Release);
                // SAFETY: Activation and rollback remain in one pinned,
                // IRQ-masked interval on `cpu`.
                let _ = unsafe { crate::hal::user::deactivate_local(backend) };
                return Err(Error::Residency(error));
            }
            Ok(backend)
        });
        let backend = match backend {
            Ok(backend) => backend,
            Err(error) => {
                ACTIVE_OWNER[cpu].store(0, Ordering::Release);
                return Err(error);
            }
        };
        Ok(ActiveNativeAddressSpace {
            owner: self,
            cpu,
            backend: Some(backend),
            identifier: Some(identifier),
        })
    }

    /// Combines architecture exit with the scheduler's completed MM detach.
    /// The proof keeps one logical run generation occupied until its return
    /// capability has been consumed, even if the syscall migrated meanwhile.
    pub(crate) fn finish_user_exit(
        &self,
        architecture: crate::hal::user::StoppedPublication,
    ) -> StoppedNativeRun {
        let Some(cpu) = crate::cpu::current_index() else {
            hyper::debug::invariant_failure("native exit without a current CPU");
        };
        if crate::hal::irq::local_enabled()
            || ACTIVE[cpu].load(Ordering::Acquire)
            || ACTIVE_OWNER[cpu].load(Ordering::Acquire) != 0
        {
            hyper::debug::invariant_failure("native exit retained local translation");
        }
        StoppedNativeRun {
            binding: architecture.binding(),
        }
    }

    /// Retires the last machine root and unregisters its software identity only
    /// after every previously resident CPU acknowledged local invalidation.
    // Returning the owner is intentional: a recoverable pre-publication Busy
    // result must preserve the address space without allocation or leakage.
    pub(crate) fn retire(owner: UniqueFallibleArc<Self>) -> Result<(), RetirementFailure> {
        let transport = crate::irq::cross_call::UserAddressSpaceTransaction::try_acquire();
        Self::retire_with_transport(owner, transport)
    }

    /// Retires an address space which has never been published to a Process.
    ///
    /// Construction rollback has no reaper-owned retry token. It therefore
    /// waits through benign mailbox contention instead of leaking a complete
    /// address space merely because another loader is updating its mappings.
    pub(crate) fn retire_unpublished(
        owner: UniqueFallibleArc<Self>,
    ) -> Result<(), RetirementFailure> {
        let transport = crate::irq::cross_call::UserAddressSpaceTransaction::acquire_waiting();
        Self::retire_with_transport(owner, transport)
    }

    fn retire_with_transport(
        mut owner: UniqueFallibleArc<Self>,
        transport: Result<crate::irq::cross_call::UserAddressSpaceTransaction, ()>,
    ) -> Result<(), RetirementFailure> {
        let mut transport = match transport {
            Ok(transport) => transport,
            Err(()) => {
                return Err(RetirementFailure {
                    error: Error::Transport,
                    owner,
                });
            }
        };
        let cut_and_image = owner.state.with(|state| {
            let cut = state.residency.begin_retirement(state.current.epoch)?;
            Ok::<_, Error>((cut, state.current.clone()))
        });
        let (cut, image) = match cut_and_image {
            Ok(value) => value,
            Err(error) => return Err(RetirementFailure { error, owner }),
        };

        // SAFETY: `owner` is consumed, admission is permanently closed, and no
        // ActiveNativeAddressSpace borrow can coexist with this move.
        let identifier = unsafe { ManuallyDrop::take(&mut owner.identifier) };
        let retiring = match identifier.begin_retirement() {
            Ok(retiring) => retiring,
            Err(_) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native translation identifier retirement is inconsistent"
            )),
        };
        let outcome = transport.execute(
            crate::irq::cross_call::UserAddressSpaceExecution {
                owner: owner.logical.id().get(),
                epoch: image.epoch,
                new_epoch: image.epoch,
                active_target: false,
                expected: &image.root,
                operation: crate::irq::cross_call::UserAddressSpaceOperation::Invalidate(
                    &image.root,
                ),
            },
            crate::cpu::online_cpu_count(),
            cut.targets(),
        );
        if outcome.rejected_cpu.is_some() || outcome.ambiguous_cpu.is_some() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native address-space final invalidation was not acknowledged"
            ));
        }
        if owner
            .state
            .with(|state| state.residency.finish_retirement(cut))
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native address-space residency retirement is inconsistent"
            ));
        }
        // SAFETY: Residency is now irreversibly retired and every target
        // acknowledged invalidating its old translations and speculative walks.
        if unsafe { retiring.complete() }.is_err() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native translation identifier completion is inconsistent"
            ));
        }

        // SAFETY: The final invalidation acknowledged before these published
        // owners are extracted and destroyed. ManuallyDrop prevents `Drop`
        // from observing or releasing the moved fields a second time.
        let state = unsafe { ManuallyDrop::take(&mut owner.state) };
        // SAFETY: The logical owner is protected by the same final quiescence.
        let logical = unsafe { ManuallyDrop::take(&mut owner.logical) };
        drop(image);
        drop(state);
        drop(logical);
        drop(owner);
        Ok(())
    }
}

/// Owned guard which prevents output mappings from changing before commit.
pub(crate) struct UserWriteReservation {
    owner: FallibleArc<NativeAddressSpace>,
    plan: Option<LogicalUserWrite>,
}

impl UserWriteReservation {
    pub(crate) fn copy_from(&self, source: &[u8]) -> Result<(), Error> {
        let Some(plan) = self.plan.as_ref() else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: completed user-write reservation reused"
            ));
        };
        self.owner.logical.write_user_reservation(plan, source)?;
        Ok(())
    }

    /// Writes an initialized prefix and leaves the rest of the reserved user
    /// range untouched.
    pub(crate) fn copy_prefix_from(&self, source: &[u8]) -> Result<(), Error> {
        let Some(plan) = self.plan.as_ref() else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: completed user-write reservation reused"
            ));
        };
        self.owner
            .logical
            .write_user_reservation_prefix(plan, source)?;
        Ok(())
    }

    pub(crate) fn complete(mut self) {
        self.release();
    }

    fn release(&mut self) {
        let plan = match self.plan.take() {
            Some(plan) => plan,
            None => return,
        };
        self.owner.logical.release_user_write(plan);
    }
}

impl Drop for UserWriteReservation {
    fn drop(&mut self) {
        self.release();
    }
}

impl Drop for NativeAddressSpace {
    fn drop(&mut self) {
        // ManuallyDrop fields intentionally retain published owners. See the
        // type-level teardown contract above.
    }
}

pub(crate) struct ActiveNativeAddressSpace<'owner> {
    owner: &'owner NativeAddressSpace,
    cpu: hyper::cpu::CpuIndex,
    backend: Option<crate::hal::user::ActiveAddressSpace<'owner>>,
    identifier: Option<IdentifierLease<HostAsid>>,
}

impl ActiveNativeAddressSpace<'_> {
    /// Epoch admitted on this CPU before entering userspace. A later shootdown
    /// may advance it; a fault from this root can then legitimately be stale.
    pub(crate) fn mapping_epoch(&self) -> u64 {
        ACTIVE_EPOCH[self.cpu].load(Ordering::Acquire)
    }

    /// Detaches before the scheduler exposes this Thread to another CPU.
    /// A concurrent page-table cut can be waiting for this CPU's acknowledgement;
    /// service its mailbox while waiting, since local IRQs are masked here.
    pub(crate) fn leave(mut self) -> Result<(), Error> {
        if crate::cpu::current_index() != Some(self.cpu) || crate::hal::irq::local_enabled() {
            return Err(Error::Hal(crate::hal::user::AddressSpaceError::InvalidCpu));
        }
        loop {
            let result = self.owner.state.with(|state| {
                let active_epoch = ACTIVE_EPOCH[self.cpu].load(Ordering::Acquire);
                match state.residency.leave(self.cpu.get(), active_epoch) {
                    Err(ResidencyError::Busy) => {
                        return Err(Error::Residency(ResidencyError::Busy));
                    }
                    Err(error) => return Err(Error::Residency(error)),
                    Ok(()) => {}
                }
                let Some(backend) = self.backend.take() else {
                    hyper::debug::invariant_failure("mm::user_space::machine::leave invariant");
                };
                if backend.cpu() != self.cpu {
                    hyper::debug::invariant_failure("mm::user_space::machine::leave invariant");
                }
                // SAFETY: IRQs are masked and this token is consumed on its
                // original CPU before the scheduler exposes the continuation.
                unsafe { crate::hal::user::deactivate_local(backend)? };
                // Hardware is detached before this pin can make the ASID reusable.
                drop(self.identifier.take());
                ACTIVE[self.cpu].store(false, Ordering::Release);
                ACTIVE_OWNER[self.cpu].store(0, Ordering::Release);
                Ok(())
            });
            match result {
                Err(Error::Residency(ResidencyError::Busy)) => {
                    #[cfg(all(
                        CONFIG_ARCH_AARCH64,
                        all(feature = "kernel-test", feature = "kernel-self-test")
                    ))]
                    self_test::observe_busy(self.owner.logical.id().get(), self.cpu);
                    // No MM or scheduler lock is held here. The updater pins
                    // itself across the closed admission cut, so it cannot be
                    // suspended by this switch on this CPU. Polling lets its
                    // remote RPC replace our root and reopen admission even
                    // though interrupt delivery is currently masked.
                    crate::irq::cross_call::service();
                    core::hint::spin_loop();
                }
                other => return other,
            }
        }
    }
}

/// Proof that both architecture publication and process translation are idle.
pub(crate) struct StoppedNativeRun {
    binding: hyper::hal::user::UserRunBinding,
}

impl StoppedNativeRun {
    pub(crate) const fn binding(&self) -> hyper::hal::user::UserRunBinding {
        self.binding
    }
}

impl Drop for ActiveNativeAddressSpace<'_> {
    fn drop(&mut self) {
        if self.backend.is_some() {
            hyper::debug::invariant_failure("mm::user_space::machine::drop invariant");
        }
    }
}

#[must_use = "prepared logical and machine roots must commit together or roll back together"]
pub(crate) struct PreparedNativeChange<'owner> {
    owner: &'owner NativeAddressSpace,
    logical: LogicalPrepared<'owner>,
    image: FallibleArc<MachineImage>,
}

impl PreparedNativeChange<'_> {
    pub(crate) fn commit(self) -> Result<MappingChange, Error> {
        let Self {
            owner,
            logical,
            image,
        } = self;
        let mut transport = crate::irq::cross_call::UserAddressSpaceTransaction::acquire_waiting()
            .map_err(|()| Error::Transport)?;
        let cut = owner
            .state
            .with(|state| state.residency.begin_update(logical.base_epoch()))?;
        #[cfg(all(
            CONFIG_ARCH_AARCH64,
            all(feature = "kernel-test", feature = "kernel-self-test")
        ))]
        self_test::after_cut(owner.logical.id().get(), cut.active());
        let committed = match logical.commit_machine() {
            Ok(committed) => committed,
            Err(error) => {
                if owner
                    .state
                    .with(|state| state.residency.abort_update(cut))
                    .is_err()
                {
                    hyper::debug::invariant_failure("mm::user_space::machine::commit invariant");
                }
                return Err(Error::Logical(error));
            }
        };
        let change = committed.change();
        let old = owner
            .state
            .with(|state| core::mem::replace(&mut state.current, image));

        let current = owner.state.with(|state| state.current.clone());
        execute_cut(
            &mut transport,
            owner.logical.id().get(),
            change,
            true,
            &old.root,
            crate::irq::cross_call::UserAddressSpaceOperation::Replace(&current.root),
            cut.active(),
        );

        let mut inactive = *cut.targets();
        for (index, active) in cut.active().iter().copied().enumerate() {
            if active {
                inactive[index] = false;
            }
        }
        execute_cut(
            &mut transport,
            owner.logical.id().get(),
            change,
            false,
            &old.root,
            crate::irq::cross_call::UserAddressSpaceOperation::Invalidate(&old.root),
            &inactive,
        );

        if owner
            .state
            .with(|state| state.residency.finish_update(cut, change.epoch))
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: native address-space residency completion is inconsistent"
            ));
        }
        // SAFETY: Both active root switches and every inactive-resident tagged
        // invalidation acknowledged before either old owner is released.
        unsafe { committed.complete_machine_retirement() };
        drop(old);
        Ok(change)
    }
}

fn execute_cut(
    transport: &mut crate::irq::cross_call::UserAddressSpaceTransaction,
    owner: u64,
    change: MappingChange,
    active_target: bool,
    expected: &crate::hal::user::PreparedAddressSpace,
    operation: crate::irq::cross_call::UserAddressSpaceOperation<'_>,
    targets: &[bool; hyper::cpu::MAX_CPUS],
) {
    if !targets.iter().copied().any(|target| target) {
        return;
    }
    let outcome = transport.execute(
        crate::irq::cross_call::UserAddressSpaceExecution {
            owner,
            epoch: change.previous_epoch,
            new_epoch: change.epoch,
            active_target,
            expected,
            operation,
        },
        crate::cpu::online_cpu_count(),
        targets,
    );
    if outcome.rejected_cpu.is_some() || outcome.ambiguous_cpu.is_some() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: committed native address-space shootdown was not acknowledged"
        ));
    }
}

fn empty_image(
    generation: u64,
    epoch: u64,
    account: DomainAccount,
) -> Result<FallibleArc<MachineImage>, Error> {
    let mut tables = TablePagePool::empty(epoch, account.clone());
    let root = {
        let mut allocator = |order| tables.allocate(order);
        // SAFETY: TablePagePool returns uniquely owned zeroed PageTable blocks
        // and is moved intact into the resulting image through acknowledged
        // retirement. The stable software owner identity is retained independently
        // of the hardware ASID lease acquired at each activation.
        unsafe { crate::hal::user::prepare_host_address_space(generation, |_| {}, &mut allocator) }
    };
    let root = table_result(root, &mut tables)?;
    finish_image(epoch, root, tables, account)
}

fn table_result(
    root: Result<crate::hal::user::PreparedAddressSpace, crate::hal::user::AddressSpaceError>,
    tables: &mut TablePagePool,
) -> Result<crate::hal::user::PreparedAddressSpace, Error> {
    if let Some(error) = tables.error.take() {
        return Err(error);
    }
    root.map_err(Error::Hal)
}

fn finish_image(
    epoch: u64,
    root: crate::hal::user::PreparedAddressSpace,
    tables: TablePagePool,
    account: DomainAccount,
) -> Result<FallibleArc<MachineImage>, Error> {
    let object_charge = account
        .try_charge(MemoryCharge {
            kernel_bytes: FallibleArc::<MachineImage>::allocation_size() as u64,
            kernel_objects: 1,
            ..MemoryCharge::default()
        })
        .map_err(Error::Resource)?;
    FallibleArc::try_new(MachineImage {
        epoch,
        root,
        _tables: tables,
        _object_charge: object_charge,
    })
    .map_err(|_| Error::Allocation)
}

pub(crate) fn service_local_rpc(
    owner: u64,
    epoch: u64,
    new_epoch: u64,
    active_target: bool,
    expected_active: crate::hal::user::LocalIdentity,
    request: crate::hal::user::LocalRequest,
) -> crate::irq::cross_call::LocalApply {
    let Some(cpu) = crate::cpu::current_index() else {
        return crate::irq::cross_call::LocalApply::Rejected;
    };
    let active = ACTIVE[cpu].load(Ordering::Acquire);
    if active_target {
        if !active
            || ACTIVE_OWNER[cpu].load(Ordering::Relaxed) != owner
            || ACTIVE_EPOCH[cpu].load(Ordering::Relaxed) != epoch
        {
            return crate::irq::cross_call::LocalApply::Rejected;
        }
        if !crate::hal::user::local_identity_is_active(expected_active) {
            return crate::irq::cross_call::LocalApply::Rejected;
        }
    } else if active && ACTIVE_OWNER[cpu].load(Ordering::Relaxed) == owner {
        return crate::irq::cross_call::LocalApply::Rejected;
    }
    // SAFETY: The closed synchronous transport retains the corresponding
    // MachineImage until this CPU acknowledges the request.
    if !unsafe { crate::hal::user::service_local_request(request) } {
        return crate::irq::cross_call::LocalApply::AppliedOrUnknown;
    }
    if active_target {
        ACTIVE_EPOCH[cpu].store(new_epoch, Ordering::Release);
    }
    crate::irq::cross_call::LocalApply::Applied
}

// SAFETY: NativeAddressSpace leaks on safe Drop and releases an old image only
// after every active/resident CPU acknowledged replacement. Its software
// identity remains registered until acknowledged final retirement.
unsafe impl hyper::hal::user::UserTranslationOwner for NativeAddressSpace {}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod self_test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use self_test::{prepare_native_entry_self_test, run_dormant_self_test};
