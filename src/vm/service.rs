// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process capability coordinators for userspace-managed virtual machines.

use crate::accounting::ResourceDomainObject;
use crate::mm::user_space::VmoObject;
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
    crate::capability::Rights,
};
use {crate::object::KernelObject, crate::object::ObjectPublication};
use {crate::process::Process, crate::process::ProcessError};

use super::objects::{
    Error as ObjectError, PendingVirtualMachine, VirtualCpuBootstrap, VirtualCpuObject,
    VirtualCpuSnapshot, VirtualMachineConfiguration, VirtualMachineCreationAuthority,
    VirtualMachineCreationLease, VirtualMachineObject, VirtualMachineSnapshot,
};

/// ABI-independent outcome classes exposed by the VM service boundary.
///
/// VM construction internals remain private to this module. Callers choose
/// their own wire or policy representation from these stable semantic classes
/// instead of coupling to registry, scheduler, or memory implementation enums.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    NotSupported,
    InvalidArgument,
    BadHandle,
    AccessDenied,
    Busy,
    WouldBlock,
    BadState,
    ResourceLimit,
    NoMemory,
    Fault,
    Io,
    Internal,
}

impl From<ObjectError> for Error {
    fn from(error: ObjectError) -> Self {
        classify_object_error(error)
    }
}

impl From<ProcessError> for Error {
    fn from(error: ProcessError) -> Self {
        classify_process_error(error)
    }
}

const fn classify_object_error(error: ObjectError) -> Error {
    match error {
        ObjectError::Allocation => Error::NoMemory,
        ObjectError::BadState => Error::BadState,
        ObjectError::InvalidConfiguration => Error::InvalidArgument,
        ObjectError::Memory(error) => classify_memory_object_error(error),
        ObjectError::Object(error) => classify_object_creation_error(error),
        ObjectError::Resource(error) => classify_resource_error(error),
        ObjectError::Scheduler(error) => classify_scheduler_error(error),
        ObjectError::UnsupportedArchitecture => Error::NotSupported,
        ObjectError::Registry(error) => classify_registry_error(error),
        ObjectError::MemoryLayout(error) => classify_guest_memory_error(error),
        ObjectError::VirtualDevice(_) | ObjectError::VirtualInterrupt(_) => Error::Internal,
        ObjectError::VirtualSerial(error) => match error {
            crate::vm::serial::Error::Memory(error) => classify_memory_object_error(error),
            crate::vm::serial::Error::Allocation => Error::NoMemory,
            crate::vm::serial::Error::AllocationSize => Error::Internal,
            crate::vm::serial::Error::Disconnected => Error::BadState,
            crate::vm::serial::Error::Resource(error) => classify_resource_error(error),
            crate::vm::serial::Error::WouldBlock => Error::Busy,
            crate::vm::serial::Error::InvalidCursor => Error::InvalidArgument,
        },
    }
}

const fn classify_process_error(error: ProcessError) -> Error {
    match error {
        ProcessError::Allocation => Error::NoMemory,
        ProcessError::Handle(error) => classify_handle_error(error),
        ProcessError::Object(error) => classify_object_creation_error(error),
        ProcessError::Lifecycle(_) | ProcessError::AddressSpaceReferenced => Error::BadState,
        ProcessError::Resource(error) => classify_resource_error(error),
        ProcessError::Scheduler(error) => classify_scheduler_error(error),
        ProcessError::TaskGroup(error) => classify_task_group_error(error),
        ProcessError::UserEntry(_) => Error::NotSupported,
        ProcessError::UserMemory(error) => classify_machine_error(error),
    }
}

const fn classify_handle_error(error: crate::capability::HandleError) -> Error {
    use crate::capability::HandleError;

    match error {
        HandleError::Allocation => Error::NoMemory,
        HandleError::InvalidHandle | HandleError::WrongObjectType => Error::BadHandle,
        HandleError::Busy | HandleError::OutstandingReservation => Error::Busy,
        HandleError::AccessDenied => Error::AccessDenied,
        HandleError::UnsupportedRights | HandleError::UnsupportedFlags => Error::InvalidArgument,
        HandleError::UnsupportedTransfer => Error::NotSupported,
        HandleError::ObjectRetired | HandleError::TableRetired => Error::BadState,
        HandleError::ActiveHandleLimit
        | HandleError::ReservationIdExhausted
        | HandleError::ReservationTooLarge
        | HandleError::TableFull => Error::ResourceLimit,
        HandleError::ObjectAlreadyActive | HandleError::EmptyReservation => Error::Internal,
    }
}

const fn classify_object_creation_error(error: crate::object::ObjectCreationError) -> Error {
    match error {
        crate::object::ObjectCreationError::Allocation => Error::NoMemory,
        crate::object::ObjectCreationError::KoidExhausted
        | crate::object::ObjectCreationError::RegistrationExhausted => Error::ResourceLimit,
    }
}

const fn classify_resource_error(error: crate::accounting::ResourceError) -> Error {
    use crate::accounting::ResourceError;

    match error {
        ResourceError::Allocation => Error::NoMemory,
        ResourceError::HierarchyTooDeep
        | ResourceError::TooManyChargeDimensions
        | ResourceError::LimitExceeded { .. }
        | ResourceError::UsageOverflow { .. }
        | ResourceError::ChildCountExhausted
        | ResourceError::DomainIdExhausted => Error::ResourceLimit,
        ResourceError::DomainInactive(_)
        | ResourceError::OutstandingUsage
        | ResourceError::ActiveChildren
        | ResourceError::RetirementNotStarted => Error::BadState,
        ResourceError::EmptyCharge | ResourceError::LimitBelowUsage { .. } => Error::Internal,
    }
}

const fn classify_task_group_error(error: crate::process::TaskGroupError) -> Error {
    match error {
        crate::process::TaskGroupError::Allocation => Error::NoMemory,
        crate::process::TaskGroupError::CounterOverflow
        | crate::process::TaskGroupError::GenerationExhausted => Error::ResourceLimit,
        crate::process::TaskGroupError::Inactive
        | crate::process::TaskGroupError::MembersRemain => Error::BadState,
        crate::process::TaskGroupError::Resource(error) => classify_resource_error(error),
    }
}

const fn classify_scheduler_error(error: crate::task::scheduler::Error) -> Error {
    use crate::task::scheduler::Error as SchedulerError;

    match error {
        SchedulerError::Allocation => Error::NoMemory,
        SchedulerError::IdentifierExhausted | SchedulerError::WaitGenerationExhausted => {
            Error::ResourceLimit
        }
        SchedulerError::EmptyCpuAffinity
        | SchedulerError::NoRegisteredCpuInAffinity
        | SchedulerError::InvalidCpuIndex
        | SchedulerError::CpuNotAllowed => Error::InvalidArgument,
        SchedulerError::CpuNotRegistered | SchedulerError::InvalidSchedulingParameters => {
            Error::InvalidArgument
        }
        SchedulerError::SchedulingPermission(_) => Error::AccessDenied,
        SchedulerError::ThreadNotFound | SchedulerError::TerminatedThread => Error::BadState,
        SchedulerError::MigrationUnsupported => Error::NotSupported,
        SchedulerError::MigrationInProgress
        | SchedulerError::ThreadTransitionInProgress
        | SchedulerError::MigrationBlockedByCpuLocalWait => Error::Busy,
        SchedulerError::Thread(error) => classify_thread_error(error),
        SchedulerError::ThreadTableGrowth
        | SchedulerError::NotInitialized
        | SchedulerError::AlreadyInitialized
        | SchedulerError::CurrentThreadMissing
        | SchedulerError::ThreadBlocked
        | SchedulerError::ThreadAlreadyQueued
        | SchedulerError::QueueCorrupted
        | SchedulerError::CannotBlockIdle
        | SchedulerError::CannotSleepWithInterruptsMasked
        | SchedulerError::CannotSleepWithPreemptionDisabled
        | SchedulerError::IrqTailRequiresInterruptsMasked
        | SchedulerError::InvalidThreadState
        | SchedulerError::IdleThreadAlreadyInstalled
        | SchedulerError::InvalidIdleTransition
        | SchedulerError::CpuAlreadyRegistered
        | SchedulerError::InvalidWaitRegistration
        | SchedulerError::PreemptionUnavailable
        | SchedulerError::PreemptionInvariant
        | SchedulerError::VmEntryUnavailable => Error::Internal,
    }
}

const fn classify_thread_error(error: crate::task::thread::Error) -> Error {
    match error {
        crate::task::thread::Error::Allocation | crate::task::thread::Error::ObjectAllocation => {
            Error::NoMemory
        }
        crate::task::thread::Error::ObjectIdentityExhausted
        | crate::task::thread::Error::ObjectRegistrationExhausted => Error::ResourceLimit,
        crate::task::thread::Error::NameTooLong
        | crate::task::thread::Error::InvalidPlacement
        | crate::task::thread::Error::VirtualInterrupt(_) => Error::Internal,
    }
}

const fn classify_memory_object_error(error: crate::mm::user_space::MemoryObjectError) -> Error {
    use crate::mm::user_space::MemoryObjectError;

    match error {
        MemoryObjectError::AlreadyPublished | MemoryObjectError::WrongVariant => Error::BadState,
        MemoryObjectError::AllocationSize => Error::Internal,
        MemoryObjectError::Object(error) => classify_object_creation_error(error),
        MemoryObjectError::Resource(error) => classify_resource_error(error),
        MemoryObjectError::AddressSpace(error) => classify_address_space_error(error),
        MemoryObjectError::Vmo(error) => classify_vmo_error(error),
    }
}

const fn classify_vmo_error(
    error: crate::mm::user_space::VmoError<
        crate::mm::user_space::KernelPageError,
        crate::accounting::ResourceError,
    >,
) -> Error {
    use crate::mm::user_space::VmoError;

    match error {
        VmoError::Account(error) => classify_resource_error(error),
        VmoError::Allocation => Error::NoMemory,
        VmoError::Backend(error) => classify_page_error(error),
        VmoError::Busy | VmoError::CowRequired | VmoError::BackingNotResident => Error::Busy,
        VmoError::BackingIo => Error::Io,
        VmoError::InvalidRange | VmoError::SizeOverflow => Error::InvalidArgument,
    }
}

const fn classify_address_space_error(
    error: crate::mm::user_space::AddressSpaceError<
        crate::mm::user_space::KernelPageError,
        crate::accounting::ResourceError,
    >,
) -> Error {
    use crate::mm::user_space::AddressSpaceError;

    match error {
        AddressSpaceError::Account(error) => classify_resource_error(error),
        AddressSpaceError::Allocation => Error::NoMemory,
        AddressSpaceError::Stopped => Error::BadState,
        AddressSpaceError::Busy | AddressSpaceError::CowRequired => Error::Busy,
        AddressSpaceError::Backend(error) => classify_page_error(error),
        AddressSpaceError::BackingIo => Error::Io,
        AddressSpaceError::IdentityExhausted => Error::ResourceLimit,
        AddressSpaceError::BackingNotResident
        | AddressSpaceError::EmptyRange
        | AddressSpaceError::InvalidAddressSpace
        | AddressSpaceError::InvalidPermissions
        | AddressSpaceError::InvalidRange
        | AddressSpaceError::NotMapped
        | AddressSpaceError::Overlap
        | AddressSpaceError::ReadDenied
        | AddressSpaceError::SizeMismatch
        | AddressSpaceError::SizeOverflow
        | AddressSpaceError::StaleMapping
        | AddressSpaceError::StaleTransaction
        | AddressSpaceError::StaleVmar
        | AddressSpaceError::WriteDenied
        | AddressSpaceError::WritableExecutableBacking => Error::Fault,
    }
}

const fn classify_page_error(error: crate::mm::user_space::KernelPageError) -> Error {
    match error {
        crate::mm::user_space::KernelPageError::Allocation(_) => Error::NoMemory,
        crate::mm::user_space::KernelPageError::AddressOverflow
        | crate::mm::user_space::KernelPageError::Range => Error::Fault,
        crate::mm::user_space::KernelPageError::MissingLinearMap
        | crate::mm::user_space::KernelPageError::Unsupported => Error::Internal,
    }
}

const fn classify_machine_error(error: crate::mm::user_space::MachineError) -> Error {
    use crate::mm::user_space::MachineError;

    match error {
        MachineError::Allocation | MachineError::Page(_) => Error::NoMemory,
        MachineError::Address(_) | MachineError::SizeOverflow => Error::Fault,
        MachineError::Logical(error) => classify_address_space_error(error),
        MachineError::Resource(error) => classify_resource_error(error),
        MachineError::Residency(_) | MachineError::Transport => Error::Busy,
        MachineError::Unsupported => Error::NotSupported,
        MachineError::Hal(_) | MachineError::Identifier(_) | MachineError::Vmo(_) => {
            Error::Internal
        }
    }
}

const fn classify_registry_error(error: super::registry::Error) -> Error {
    match error {
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        super::registry::Error::AdministrativeStopUnsupported => Error::NotSupported,
        super::registry::Error::Allocation => Error::NoMemory,
        super::registry::Error::IdentityExhausted | super::registry::Error::RegistryFull => {
            Error::ResourceLimit
        }
        super::registry::Error::Resource(error) => classify_resource_error(error),
        super::registry::Error::EndpointClosed
        | super::registry::Error::InvalidReservation
        | super::registry::Error::NotInstalled
        | super::registry::Error::Quiescing
        | super::registry::Error::Scheduler
        | super::registry::Error::StaleIdentity
        | super::registry::Error::UnknownVcpu => Error::Internal,
    }
}

const fn classify_guest_memory_error(error: super::memory::Error) -> Error {
    match error {
        super::memory::Error::TableMaintenance => Error::WouldBlock,
        super::memory::Error::Identifier(crate::mm::translation_id::Error::Busy) => Error::Busy,
        super::memory::Error::Identifier(crate::mm::translation_id::Error::Allocation) => {
            Error::NoMemory
        }
        super::memory::Error::Identifier(_) => Error::Internal,
        super::memory::Error::AddressOverflow | super::memory::Error::InvalidRange => {
            Error::InvalidArgument
        }
        super::memory::Error::Allocation(_) | super::memory::Error::MetadataAllocation => {
            Error::NoMemory
        }
        super::memory::Error::Resource(error) => classify_resource_error(error),
        super::memory::Error::Registry(error) => classify_registry_error(error),
        super::memory::Error::Cache(_)
        | super::memory::Error::MemoryObject
        | super::memory::Error::InvalidCpu
        | super::memory::Error::Poisoned
        | super::memory::Error::Residency(_)
        | super::memory::Error::Stage2(_) => Error::Internal,
    }
}

/// Immutable board metadata, separate from per-VM mutable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualMachinePlatformInfo {
    pub architecture: u32,
    pub platform_profile: u32,
    pub counter_frequency_hz: u64,
    pub riscv_isa: u64,
    pub aarch64_gic_version: u64,
}

pub(crate) fn platform_info(
    lease: &InvocationReceiver,
    platform_profile: u32,
) -> Result<VirtualMachinePlatformInfo, Error> {
    // Resolution only borrows the lease: failed inspection cannot consume the
    // caller's one-shot creation authority or charge a VM reservation.
    let _lease = lease
        .resolve::<VirtualMachineCreationLease>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    selected_platform_info(platform_profile)
}

fn selected_platform_info(platform_profile: u32) -> Result<VirtualMachinePlatformInfo, Error> {
    use hyper::abi::native::*;
    if !crate::hal::vm::userspace_vm_lifecycle_available() || !crate::hal::vm::platform_supported()
    {
        return Err(Error::NotSupported);
    }
    let architecture = crate::hal::vm::guest_architecture_abi();
    let (expected_profile, riscv_isa) = match u64::from(architecture) {
        HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_AARCH64 => {
            (HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE, 0)
        }
        HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_RISCV64 => {
            if !crate::hal::vm::riscv_guest_baseline_available() {
                return Err(Error::NotSupported);
            }
            (
                HYPER_NATIVE_VIRTUAL_PLATFORM_RISCV64_REFERENCE,
                HYPER_NATIVE_RISCV_ISA_I
                    | HYPER_NATIVE_RISCV_ISA_M
                    | HYPER_NATIVE_RISCV_ISA_A
                    | HYPER_NATIVE_RISCV_ISA_F
                    | HYPER_NATIVE_RISCV_ISA_D
                    | HYPER_NATIVE_RISCV_ISA_C
                    | HYPER_NATIVE_RISCV_ISA_ZICSR
                    | HYPER_NATIVE_RISCV_ISA_ZIFENCEI
                    | HYPER_NATIVE_RISCV_ISA_SSTC,
            )
        }
        _ => return Err(Error::NotSupported),
    };
    if u64::from(platform_profile) != expected_profile {
        return Err(Error::NotSupported);
    }
    let counter_frequency_hz =
        crate::hal::time::counter_frequency_hz().map_err(|_| Error::Internal)?;
    if counter_frequency_hz == 0 {
        return Err(Error::Internal);
    }
    Ok(VirtualMachinePlatformInfo {
        architecture,
        platform_profile,
        counter_frequency_hz,
        riscv_isa,
        aarch64_gic_version: u64::from(crate::hal::vm::guest_gic_version()),
    })
}

pub(crate) fn derive_creation_lease(
    process: &Process,
    authority: &InvocationReceiver,
    domain: HandleValue,
) -> Result<HandleValue, Error> {
    let authority = authority
        .resolve::<VirtualMachineCreationAuthority>(
            Rights::DERIVE.union(Rights::CREATE_VIRTUAL_MACHINE),
        )
        .map_err(crate::process::ProcessError::from)?;
    let domain =
        process.resolve_handle::<ResourceDomainObject>(domain, Rights::RESOURCE_DOMAIN_SPONSOR)?;
    let lease = authority.object().derive(domain.object().domain())?;
    Ok(process.create_object(
        lease,
        Rights::TRANSFER
            .union(Rights::INSPECT)
            .union(Rights::CREATE_VIRTUAL_MACHINE)
            .union(
                authority
                    .rights()
                    .intersection(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
    )?)
}

/// Replaces one creation lease with a pending VM in the caller's handle table.
///
/// Reserve the output and claim the input before constructing the object.
/// Recoverable errors restore the lease and abort the reservation; publication
/// commits both handle ownership and accounting together.
pub(crate) fn create_pending(
    process: &Process,
    lease: &InvocationReceiver,
    configuration: VirtualMachineConfiguration,
) -> Result<HandleValue, Error> {
    let lease_object = lease
        .resolve::<VirtualMachineCreationLease>(Rights::CREATE_VIRTUAL_MACHINE)
        .map_err(crate::process::ProcessError::from)?;
    let domain = lease_object.object().domain().clone();
    let output = process.reserve_handles::<1>()?;
    let consumption = match process.prepare_handle_consumption(
        lease.handle(),
        Rights::CREATE_VIRTUAL_MACHINE,
        VirtualMachineCreationLease::KIND,
        lease_object.koid(),
    ) {
        Ok(consumption) => consumption,
        Err(error) => {
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let pending = match PendingVirtualMachine::try_new(configuration, &domain) {
        Ok(pending) => pending,
        Err(error) => {
            consumption.rollback();
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let prepared = match prepare_handle(
        pending,
        Rights::TRANSFER
            .union(Rights::INSPECT)
            .union(Rights::WRITE)
            .union(Rights::START)
            .union(Rights::REQUEST_STOP)
            .union(
                lease_object
                    .rights()
                    .intersection(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            consumption.rollback();
            process.abort_handles(output);
            return Err(error);
        }
    };
    Ok(consumption.commit_replacement(output, [prepared])[0])
}

pub(crate) fn set_memory(
    process: &Process,
    pending: &InvocationReceiver,
    vmo: HandleValue,
) -> Result<(), Error> {
    let pending = pending
        .resolve::<PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let vmo = process
        .resolve_handle::<VmoObject>(vmo, Rights::READ.union(Rights::WRITE).union(Rights::MAP))?;
    pending.object().set_memory(vmo.object())?;
    Ok(())
}

pub(crate) fn set_bootstrap(
    pending: &InvocationReceiver,
    bootstrap: VirtualCpuBootstrap,
) -> Result<(), Error> {
    let pending = pending
        .resolve::<PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    pending.object().set_bootstrap(bootstrap)?;
    Ok(())
}

pub(crate) fn set_virtual_serial(
    process: &Process,
    pending: &InvocationReceiver,
    serial: HandleValue,
) -> Result<(), Error> {
    let pending = pending
        .resolve::<PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let required = Rights::ASSIGN_DEVICE.union(Rights::TRANSFER);
    let serial_object =
        process.resolve_handle::<crate::vm::serial::VirtualSerial>(serial, required)?;
    let consumption = process.prepare_handle_consumption(
        serial,
        required,
        crate::vm::serial::VirtualSerial::KIND,
        serial_object.koid(),
    )?;
    let binding = serial_object.into_operation_pin().into_vm_device_binding();
    match pending.object().set_virtual_serial(binding) {
        Ok(()) => {
            consumption.commit_and_release();
            Ok(())
        }
        Err(error) => {
            consumption.rollback();
            Err(error.into())
        }
    }
}

pub(crate) fn create_virtual_serial(process: &Process) -> Result<HandleValue, Error> {
    let serial = crate::vm::serial::VirtualSerial::try_new(&process.resource_domain())
        .map_err(ObjectError::from)?;
    Ok(process.create_object(
        serial,
        <crate::vm::serial::VirtualSerial as KernelObject>::SUPPORTED_RIGHTS,
    )?)
}

pub(crate) fn register_virtual_serial_output(
    process: &Process,
    value: &InvocationReceiver,
    buffer: HandleValue,
) -> Result<(), Error> {
    let serial = value
        .resolve::<crate::vm::serial::VirtualSerial>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let buffer = process.resolve_handle::<VmoObject>(
        buffer,
        Rights::READ.union(Rights::WRITE).union(Rights::MAP),
    )?;
    serial
        .object()
        .register_output(buffer.object(), &process.resource_domain())
        .map_err(ObjectError::from)
        .map_err(Into::into)
}

pub(crate) fn acknowledge_virtual_serial_output(
    value: &InvocationReceiver,
    consumed: u64,
) -> Result<(), Error> {
    let serial = value
        .resolve::<crate::vm::serial::VirtualSerial>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    serial
        .object()
        .acknowledge_output(consumed)
        .map_err(ObjectError::from)
        .map_err(Into::into)
}

pub(crate) fn seal(pending: &InvocationReceiver) -> Result<(), Error> {
    let pending = pending
        .resolve::<PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    pending.object().seal()?;
    Ok(())
}

/// Publishes a sealed VM and replaces its pending handle with VM/vCPU handles.
///
/// Prepare output ownership before taking the sealed resources. After that
/// irreversible take, registry publication must succeed: failure is an internal
/// invariant violation, not a recoverable error with an intact pending VM.
/// The boot vCPU remains dormant until a separate start request.
pub(crate) fn install(
    process: &Process,
    pending_value: &InvocationReceiver,
) -> Result<[HandleValue; 2], Error> {
    let pending = pending_value
        .resolve::<PendingVirtualMachine>(Rights::START)
        .map_err(crate::process::ProcessError::from)?;
    let output = process.reserve_handles::<2>()?;
    let consumption = match process.prepare_handle_consumption(
        pending_value.handle(),
        Rights::START,
        PendingVirtualMachine::KIND,
        pending.koid(),
    ) {
        Ok(consumption) => consumption,
        Err(error) => {
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let domain = pending.object().resource_domain();
    let lifecycle = match pending.object().installed_lifecycle() {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            consumption.rollback();
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let vm_object = match VirtualMachineObject::try_new(lifecycle.clone(), &domain) {
        Ok(object) => object,
        Err(error) => {
            consumption.rollback();
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let vm_handle = match prepare_handle(
        vm_object,
        Rights::TRANSFER
            .union(Rights::INSPECT)
            .union(Rights::WRITE)
            .union(Rights::WAIT)
            .union(Rights::REQUEST_STOP)
            .union(
                pending
                    .rights()
                    .intersection(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            consumption.rollback();
            process.abort_handles(output);
            return Err(error);
        }
    };
    let vcpu_object = match VirtualCpuObject::try_new(lifecycle.clone(), 0, &domain) {
        Ok(object) => object,
        Err(error) => {
            drop(vm_handle);
            consumption.rollback();
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let vcpu_handle = match prepare_handle(
        vcpu_object,
        Rights::TRANSFER
            .union(Rights::INSPECT)
            .union(Rights::WRITE)
            .union(Rights::WAIT)
            .union(Rights::START)
            .union(Rights::SCHED_LOWER)
            .union(
                pending
                    .rights()
                    .intersection(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            drop(vm_handle);
            consumption.rollback();
            process.abort_handles(output);
            return Err(error);
        }
    };
    let prepared = match pending.object().take_prepared() {
        Ok(prepared) => prepared,
        Err(error) => {
            drop((vm_handle, vcpu_handle));
            consumption.rollback();
            process.abort_handles(output);
            return Err(error.into());
        }
    };
    let installed = match prepared.install() {
        Ok(installed) => installed,
        Err(error) => {
            // Hardware domain allocation and activation can still fail after
            // sealing. The installation transaction has withdrawn admission;
            // keep the now exhausted pending handle available for abort/close.
            drop((vm_handle, vcpu_handle));
            consumption.rollback();
            process.abort_handles(output);
            return Err(classify_registry_error(error));
        }
    };
    drop(installed.publish_handle_lifecycle());
    Ok(consumption.commit_replacement(output, [vm_handle, vcpu_handle]))
}

pub(crate) fn abort(process: &Process, pending: &InvocationReceiver) -> Result<(), Error> {
    let resolved = pending
        .resolve::<PendingVirtualMachine>(Rights::REQUEST_STOP)
        .map_err(crate::process::ProcessError::from)?;
    let consumption = process.prepare_handle_consumption(
        pending.handle(),
        Rights::REQUEST_STOP,
        PendingVirtualMachine::KIND,
        resolved.koid(),
    )?;
    consumption.commit_and_release();
    Ok(())
}

pub(crate) fn request_stop(machine: &InvocationReceiver) -> Result<(), Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::REQUEST_STOP)
        .map_err(crate::process::ProcessError::from)?;
    machine.object().request_stop();
    Ok(())
}

pub(crate) fn machine_info(
    machine: &InvocationReceiver,
) -> Result<(VirtualMachineConfiguration, VirtualMachineSnapshot), Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    Ok((
        machine.object().configuration(),
        machine.object().snapshot(),
    ))
}

pub(crate) fn vcpu_info(vcpu: &InvocationReceiver) -> Result<VirtualCpuSnapshot, Error> {
    let vcpu = vcpu
        .resolve::<VirtualCpuObject>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    Ok(vcpu.object().snapshot())
}

pub(crate) fn start_vcpu(vcpu: &InvocationReceiver) -> Result<(), Error> {
    let vcpu = vcpu
        .resolve::<VirtualCpuObject>(Rights::START)
        .map_err(crate::process::ProcessError::from)?;
    vcpu.object().start()?;
    Ok(())
}

pub(crate) fn set_vcpu_affinity(
    vcpu: &InvocationReceiver,
    affinity: crate::task::policy::CpuMask,
) -> Result<(), Error> {
    let vcpu = vcpu
        .resolve::<VirtualCpuObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    vcpu.object().set_affinity(affinity)?;
    Ok(())
}

fn prepare_handle<T: crate::object::UserExportableObject>(
    payload: T,
    rights: Rights,
) -> Result<PreparedHandle, Error> {
    let publication = ObjectPublication::try_new(payload).map_err(ObjectError::from)?;
    PreparedHandle::try_from_new_object(publication, rights, HandleFlags::NONE)
        .map_err(ProcessError::from)
        .map_err(Into::into)
}

/// Opens a capability for an already configured vCPU identity. Handle creation
/// does not allocate or start another scheduler Thread.
pub(crate) fn open_vcpu(
    process: &Process,
    machine: &InvocationReceiver,
    id: u32,
) -> Result<HandleValue, Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let object =
        VirtualCpuObject::try_new(machine.object().owner(), id, &process.resource_domain())?;
    let prepared = prepare_handle(
        object,
        Rights::TRANSFER
            .union(Rights::INSPECT)
            .union(Rights::WRITE)
            .union(Rights::WAIT)
            .union(Rights::START)
            .union(Rights::SCHED_LOWER)
            .union(
                machine
                    .rights()
                    .intersection(crate::task::scheduling_authority::SCHEDULING_RIGHTS),
            ),
    )?;
    let reservation = process.reserve_handles::<1>()?;
    process
        .publish_handles(reservation, [prepared])
        .map(|values| values[0])
        .map_err(|failure| failure.error.into())
}

pub(crate) fn pending_power_request(
    machine: &InvocationReceiver,
) -> Result<Option<hyper_vm_models::arm::psci::Request>, Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    Ok(machine.object().owner().pending_power_request())
}

pub(crate) fn complete_power_request(
    machine: &InvocationReceiver,
    id: u64,
    accept: bool,
) -> Result<(), Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    machine
        .object()
        .owner()
        .complete_power_request(id, accept)
        .map_err(|_| Error::BadState)
}

/// Registers an immutable device aperture before the VM starts.
pub(crate) fn register_mmio(
    machine: &InvocationReceiver,
    base: u64,
    length: u64,
    device: u64,
) -> Result<(), Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    machine
        .object()
        .owner()
        .register_mmio(base, length, device)
        .map_err(|_| Error::BadState)
}

pub(crate) fn pending_mmio(
    vcpu: &InvocationReceiver,
) -> Result<Option<hyper_vm_models::device::mmio::Request>, Error> {
    let vcpu = vcpu
        .resolve::<VirtualCpuObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    vcpu.object().pending_mmio().map_err(Into::into)
}

pub(crate) fn complete_mmio(
    vcpu: &InvocationReceiver,
    id: u64,
    action: hyper_vm_models::exit::MmioAction,
) -> Result<(), Error> {
    let vcpu = vcpu
        .resolve::<VirtualCpuObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    vcpu.object().complete_mmio(id, action).map_err(Into::into)
}

pub(crate) fn create_guest_memory(
    process: &Process,
    vmo: &InvocationReceiver,
) -> Result<HandleValue, Error> {
    use super::objects::GuestMemoryObject;
    let vmo = vmo
        .resolve::<VmoObject>(Rights::READ.union(Rights::WRITE).union(Rights::MAP))
        .map_err(crate::process::ProcessError::from)?;
    let object = GuestMemoryObject::try_new(vmo.object(), &process.resource_domain())?;
    let prepared = prepare_handle(object, GuestMemoryObject::SUPPORTED_RIGHTS)?;
    let reservation = process.reserve_handles::<1>()?;
    process
        .publish_handles(reservation, [prepared])
        .map(|values| values[0])
        .map_err(|failure| failure.error.into())
}

pub(crate) fn map_guest_memory(
    process: &Process,
    pending: &InvocationReceiver,
    memory: HandleValue,
    guest_offset: u64,
    source_offset: u64,
    length: u64,
) -> Result<(), Error> {
    let pending = pending
        .resolve::<PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let memory =
        process.resolve_handle::<super::objects::GuestMemoryObject>(memory, Rights::MAP)?;
    let region = super::memory::backing::Region::new(
        guest_offset,
        source_offset,
        length,
        memory.object().backing(),
    )
    .map_err(ObjectError::from)?;
    pending.object().map_memory(region).map_err(Into::into)
}
