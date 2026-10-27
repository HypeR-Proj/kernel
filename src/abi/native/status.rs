// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native semantic-error to wire-status translation and result construction.

use crate::capability::handle_wire;

use hyper::abi::native::{
    HYPER_NATIVE_STATUS_ACCESS_DENIED, HYPER_NATIVE_STATUS_BAD_HANDLE,
    HYPER_NATIVE_STATUS_BAD_STATE, HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL, HYPER_NATIVE_STATUS_BUSY,
    HYPER_NATIVE_STATUS_CANCELLED, HYPER_NATIVE_STATUS_FAULT, HYPER_NATIVE_STATUS_INTERNAL,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_STATUS_IO_ERROR,
    HYPER_NATIVE_STATUS_NO_MEMORY, HYPER_NATIVE_STATUS_NOT_FOUND,
    HYPER_NATIVE_STATUS_NOT_SUPPORTED, HYPER_NATIVE_STATUS_OK, HYPER_NATIVE_STATUS_PEER_CLOSED,
    HYPER_NATIVE_STATUS_RESOURCE_LIMIT, HYPER_NATIVE_STATUS_TIMED_OUT,
    HYPER_NATIVE_STATUS_WOULD_BLOCK, HyperNativeStatus, NativeResult,
};

use crate::accounting::ResourceError;
use crate::task::TimedWaitError;
use {crate::capability::HandleError, crate::capability::HandleValue};
use {crate::fs::vfs::VfsError, crate::fs::vfs::VfsServiceError};
use {
    crate::ipc::ByteChannelError, crate::ipc::ByteChannelServiceError,
    crate::ipc::CapabilityChannelError, crate::ipc::CapabilityChannelServiceError,
};
use {
    crate::mm::user_space::AddressError, crate::mm::user_space::AddressSpaceError,
    crate::mm::user_space::MachineError, crate::mm::user_space::MemoryServiceError,
};
use {
    crate::object::EventError, crate::object::ObjectCreationError, crate::object::ObjectWaitError,
    crate::object::SignalWaitError,
};
use {
    crate::process::ChildProcessStartError, crate::process::ProcessBuilderError,
    crate::process::ProcessError,
};

use super::services::{ConsoleServiceError, ObjectServiceError, ProcessBuilderServiceError};

pub(super) fn handle_result(result: Result<HandleValue, HyperNativeStatus>) -> NativeResult {
    match result {
        Ok(value) => success([handle_wire::to_abi(value), 0]),
        Err(status) => failure(status),
    }
}

pub(super) fn scan_result(result: Result<(usize, u64), HyperNativeStatus>) -> NativeResult {
    match result {
        Ok((count, next)) => match u64::try_from(count) {
            Ok(count) => success([count, next]),
            Err(_) => failure(HYPER_NATIVE_STATUS_INTERNAL),
        },
        Err(status) => failure(status),
    }
}

pub(super) fn status_only(result: Result<(), HyperNativeStatus>) -> NativeResult {
    match result {
        Ok(()) => success([0, 0]),
        Err(status) => failure(status),
    }
}

pub(super) fn info_result(result: Result<u64, HyperNativeStatus>) -> NativeResult {
    match result {
        Ok(supported_size) => success([supported_size, 0]),
        Err(status) => failure(status),
    }
}

pub(super) fn console_io_result(result: Result<usize, HyperNativeStatus>) -> NativeResult {
    match result {
        Ok(actual) => match u64::try_from(actual) {
            Ok(actual) => success([actual, 0]),
            Err(_) => failure(HYPER_NATIVE_STATUS_INTERNAL),
        },
        Err(status) => failure(status),
    }
}

pub(super) const fn success(values: [u64; 2]) -> NativeResult {
    NativeResult::new(HYPER_NATIVE_STATUS_OK, values)
}

pub(super) const fn failure(status: HyperNativeStatus) -> NativeResult {
    NativeResult::new(status, [0, 0])
}

pub(super) fn status_from_process_error(error: ProcessError) -> HyperNativeStatus {
    match error {
        ProcessError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ProcessError::Handle(error) => status_from_handle_error(error),
        ProcessError::Object(error) => status_from_object_creation_error(error),
        ProcessError::Lifecycle(_) | ProcessError::AddressSpaceReferenced => {
            HYPER_NATIVE_STATUS_BAD_STATE
        }
        ProcessError::Resource(error) => status_from_resource_error(error),
        ProcessError::Scheduler(error) => status_from_scheduler_error(error),
        ProcessError::TaskGroup(error) => status_from_task_group_error(error),
        ProcessError::UserEntry(crate::hal::user::UserEntryError::InvalidContext) => {
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT
        }
        ProcessError::UserEntry(_) => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ProcessError::UserMemory(error) => status_from_machine_error(error),
    }
}

pub(super) fn status_from_inspection_error(
    error: crate::diagnostics::inspect::Error,
) -> HyperNativeStatus {
    match error {
        crate::diagnostics::inspect::Error::AccessDenied => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        crate::diagnostics::inspect::Error::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        crate::diagnostics::inspect::Error::Details(error) => match error {
            crate::object::diagnostics::DetailError::Unsupported => {
                HYPER_NATIVE_STATUS_NOT_SUPPORTED
            }
            crate::object::diagnostics::DetailError::InvalidCursor => {
                HYPER_NATIVE_STATUS_INVALID_ARGUMENT
            }
            crate::object::diagnostics::DetailError::Stale => HYPER_NATIVE_STATUS_NOT_FOUND,
        },
        crate::diagnostics::inspect::Error::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        crate::diagnostics::inspect::Error::NotFound => HYPER_NATIVE_STATUS_NOT_FOUND,
        crate::diagnostics::inspect::Error::Object(error) => {
            status_from_object_creation_error(error)
        }
        crate::diagnostics::inspect::Error::Process(error) => status_from_process_error(error),
        crate::diagnostics::inspect::Error::Resource(error) => status_from_resource_error(error),
        crate::diagnostics::inspect::Error::Scheduler(error) => status_from_scheduler_error(error),
        crate::diagnostics::inspect::Error::Unavailable => HYPER_NATIVE_STATUS_BAD_STATE,
        crate::diagnostics::inspect::Error::InconsistentAccounting => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) const fn status_from_scheduler_error(
    error: crate::task::scheduler::Error,
) -> HyperNativeStatus {
    use crate::task::scheduler::Error;

    match error {
        Error::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        Error::IdentifierExhausted | Error::WaitGenerationExhausted => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        Error::EmptyCpuAffinity
        | Error::NoRegisteredCpuInAffinity
        | Error::InvalidCpuIndex
        | Error::CpuNotAllowed => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        Error::InvalidThreadState => HYPER_NATIVE_STATUS_BAD_STATE,
        Error::SchedulingPermission(_) => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        Error::InvalidSchedulingParameters => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        Error::ThreadTableGrowth
        | Error::NotInitialized
        | Error::AlreadyInitialized
        | Error::CurrentThreadMissing
        | Error::ThreadNotFound
        | Error::TerminatedThread
        | Error::ThreadBlocked
        | Error::ThreadAlreadyQueued
        | Error::QueueCorrupted
        | Error::CannotBlockIdle
        | Error::CannotSleepWithInterruptsMasked
        | Error::CannotSleepWithPreemptionDisabled
        | Error::IrqTailRequiresInterruptsMasked
        | Error::IdleThreadAlreadyInstalled
        | Error::InvalidIdleTransition
        | Error::CpuAlreadyRegistered
        | Error::CpuNotRegistered
        | Error::MigrationUnsupported
        | Error::MigrationInProgress
        | Error::ThreadTransitionInProgress
        | Error::InvalidWaitRegistration
        | Error::MigrationBlockedByCpuLocalWait
        | Error::PreemptionUnavailable
        | Error::PreemptionInvariant
        | Error::VmEntryUnavailable => HYPER_NATIVE_STATUS_INTERNAL,
        Error::Thread(error) => status_from_thread_error(error),
    }
}

pub(super) const fn status_from_thread_error(
    error: crate::task::thread::Error,
) -> HyperNativeStatus {
    use crate::task::thread::Error;

    match error {
        Error::Allocation | Error::ObjectAllocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        Error::ObjectIdentityExhausted | Error::ObjectRegistrationExhausted => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        Error::NameTooLong | Error::InvalidPlacement | Error::VirtualInterrupt(_) => {
            HYPER_NATIVE_STATUS_INTERNAL
        }
    }
}

pub(super) const fn status_from_task_group_error(
    error: crate::process::TaskGroupError,
) -> HyperNativeStatus {
    use crate::process::TaskGroupError;

    match error {
        TaskGroupError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        TaskGroupError::CounterOverflow | TaskGroupError::GenerationExhausted => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        TaskGroupError::Inactive | TaskGroupError::MembersRemain => HYPER_NATIVE_STATUS_BAD_STATE,
        TaskGroupError::Resource(error) => status_from_resource_error(error),
    }
}

pub(super) const fn status_from_object_creation_error(
    error: ObjectCreationError,
) -> HyperNativeStatus {
    match error {
        ObjectCreationError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ObjectCreationError::KoidExhausted | ObjectCreationError::RegistrationExhausted => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
    }
}

pub(super) fn status_from_object_service_error(error: ObjectServiceError) -> HyperNativeStatus {
    match error {
        ObjectServiceError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ObjectServiceError::Process(error) => status_from_process_error(error),
        ObjectServiceError::Event(error) => status_from_event_error(error),
        ObjectServiceError::WaitSet(error) => match error {
            crate::object::WaitSetError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
            crate::object::WaitSetError::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
            crate::object::WaitSetError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
            crate::object::WaitSetError::Full => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
            crate::object::WaitSetError::Missing => HYPER_NATIVE_STATUS_NOT_FOUND,
            crate::object::WaitSetError::Busy => HYPER_NATIVE_STATUS_BUSY,
            crate::object::WaitSetError::Closed => HYPER_NATIVE_STATUS_CANCELLED,
            crate::object::WaitSetError::TimedOut => HYPER_NATIVE_STATUS_TIMED_OUT,
            crate::object::WaitSetError::Resource(error) => status_from_resource_error(error),
            crate::object::WaitSetError::Wait(error) => status_from_object_wait_error(error),
        },
        ObjectServiceError::Wait(error) => status_from_object_wait_error(error),
    }
}

pub(super) fn status_from_byte_channel_service_error(
    error: ByteChannelServiceError,
) -> HyperNativeStatus {
    match error {
        ByteChannelServiceError::InvalidBuffer => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ByteChannelServiceError::Process(error) => status_from_process_error(error),
        ByteChannelServiceError::Channel(error) => status_from_byte_channel_error(error),
    }
}

pub(super) fn status_from_capability_channel_service_error(
    error: CapabilityChannelServiceError,
) -> HyperNativeStatus {
    match error {
        CapabilityChannelServiceError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        CapabilityChannelServiceError::Process(error) => status_from_process_error(error),
        CapabilityChannelServiceError::Channel(error) => {
            status_from_capability_channel_error(error)
        }
        CapabilityChannelServiceError::Wait(error) => status_from_object_wait_error(error),
    }
}

pub(super) const fn status_from_capability_channel_error(
    error: CapabilityChannelError,
) -> HyperNativeStatus {
    match error {
        CapabilityChannelError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        CapabilityChannelError::AllocationSize | CapabilityChannelError::Internal => {
            HYPER_NATIVE_STATUS_INTERNAL
        }
        CapabilityChannelError::EndpointClosed | CapabilityChannelError::BadState => {
            HYPER_NATIVE_STATUS_BAD_STATE
        }
        CapabilityChannelError::PeerClosed => HYPER_NATIVE_STATUS_PEER_CLOSED,
        CapabilityChannelError::WouldBlock => HYPER_NATIVE_STATUS_WOULD_BLOCK,
        CapabilityChannelError::ReceiverQueueFull | CapabilityChannelError::ResourceLimit => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        CapabilityChannelError::BufferTooSmall { .. } => HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
        CapabilityChannelError::InvalidHandle | CapabilityChannelError::WrongObjectType => {
            HYPER_NATIVE_STATUS_BAD_HANDLE
        }
        CapabilityChannelError::AccessDenied => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        CapabilityChannelError::UnsupportedTransfer => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        CapabilityChannelError::Busy => HYPER_NATIVE_STATUS_BUSY,
        CapabilityChannelError::UserMemoryFault => HYPER_NATIVE_STATUS_FAULT,
        CapabilityChannelError::TimedOut => HYPER_NATIVE_STATUS_TIMED_OUT,
        CapabilityChannelError::Cancelled => HYPER_NATIVE_STATUS_CANCELLED,
        CapabilityChannelError::Resource(error) => status_from_resource_error(error),
    }
}

pub(super) fn status_from_process_builder_service_error(
    error: ProcessBuilderServiceError,
) -> HyperNativeStatus {
    match error {
        ProcessBuilderServiceError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ProcessBuilderServiceError::Process(error) => status_from_process_error(error),
        ProcessBuilderServiceError::Builder(error) => status_from_process_builder_error(error),
        ProcessBuilderServiceError::Start(error) => status_from_process_builder_start_error(error),
    }
}

pub(super) fn status_from_process_builder_start_error(
    error: ProcessBuilderError<ChildProcessStartError>,
) -> HyperNativeStatus {
    match error {
        ProcessBuilderError::Transaction(error) => status_from_child_process_start_error(error),
        ProcessBuilderError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ProcessBuilderError::AlreadySealed
        | ProcessBuilderError::AlreadyStarted
        | ProcessBuilderError::Aborted
        | ProcessBuilderError::MissingName
        | ProcessBuilderError::NotSealed => HYPER_NATIVE_STATUS_BAD_STATE,
        ProcessBuilderError::Busy => HYPER_NATIVE_STATUS_BUSY,
        ProcessBuilderError::ExecutableFile(error) => status_from_vfs_error(error),
        ProcessBuilderError::DuplicateStartupPurpose
        | ProcessBuilderError::InvalidAffinity
        | ProcessBuilderError::InvalidScheduling
        | ProcessBuilderError::InvalidStartupPurpose
        | ProcessBuilderError::InvalidString => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ProcessBuilderError::Image(error) => status_from_loader_error(error),
        ProcessBuilderError::Object(error) => status_from_object_creation_error(error),
        ProcessBuilderError::Process(error) => status_from_process_error(error),
        ProcessBuilderError::Resource(error) => status_from_resource_error(error),
        ProcessBuilderError::StartupHandleLimit | ProcessBuilderError::DataLimit => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        ProcessBuilderError::UnsupportedStartupKind => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ProcessBuilderError::Retention(error) => status_from_retention_error(error),
    }
}

pub(super) fn status_from_process_builder_error(
    error: ProcessBuilderError<()>,
) -> HyperNativeStatus {
    match error {
        ProcessBuilderError::Transaction(()) => HYPER_NATIVE_STATUS_INTERNAL,
        ProcessBuilderError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ProcessBuilderError::AlreadySealed
        | ProcessBuilderError::AlreadyStarted
        | ProcessBuilderError::Aborted
        | ProcessBuilderError::MissingName
        | ProcessBuilderError::NotSealed => HYPER_NATIVE_STATUS_BAD_STATE,
        ProcessBuilderError::Busy => HYPER_NATIVE_STATUS_BUSY,
        ProcessBuilderError::ExecutableFile(error) => status_from_vfs_error(error),
        ProcessBuilderError::DuplicateStartupPurpose
        | ProcessBuilderError::InvalidAffinity
        | ProcessBuilderError::InvalidScheduling
        | ProcessBuilderError::InvalidStartupPurpose
        | ProcessBuilderError::InvalidString => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ProcessBuilderError::Image(error) => status_from_loader_error(error),
        ProcessBuilderError::Object(error) => status_from_object_creation_error(error),
        ProcessBuilderError::Process(error) => status_from_process_error(error),
        ProcessBuilderError::Resource(error) => status_from_resource_error(error),
        ProcessBuilderError::StartupHandleLimit | ProcessBuilderError::DataLimit => {
            HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        }
        ProcessBuilderError::UnsupportedStartupKind => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ProcessBuilderError::Retention(error) => status_from_retention_error(error),
    }
}

pub(super) fn status_from_child_process_start_error(
    error: ChildProcessStartError,
) -> HyperNativeStatus {
    match error {
        ChildProcessStartError::Process(error) => status_from_process_error(error),
        ChildProcessStartError::Bootstrap(error) => status_from_bootstrap_error(error),
        ChildProcessStartError::TaskObject(error) => status_from_task_object_error(error),
        ChildProcessStartError::VmarObject(error) => status_from_memory_object_error(error),
    }
}

pub(super) const fn status_from_task_object_error(
    error: crate::process::TaskObjectError,
) -> HyperNativeStatus {
    match error {
        crate::process::TaskObjectError::AlreadyPublished => HYPER_NATIVE_STATUS_BAD_STATE,
        crate::process::TaskObjectError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        crate::process::TaskObjectError::Object(error) => status_from_object_creation_error(error),
        crate::process::TaskObjectError::Resource(error) => status_from_resource_error(error),
        crate::process::TaskObjectError::TaskGroup(_) => HYPER_NATIVE_STATUS_BAD_STATE,
    }
}

pub(super) fn status_from_hierarchy_error(
    error: crate::process::hierarchy::Error,
) -> HyperNativeStatus {
    match error {
        crate::process::hierarchy::Error::NotSupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        crate::process::hierarchy::Error::Process(error) => status_from_process_error(error),
        crate::process::hierarchy::Error::ResourceDomain(error) => match error {
            crate::accounting::ResourceDomainObjectError::AlreadyPublished => {
                HYPER_NATIVE_STATUS_BAD_STATE
            }
            crate::accounting::ResourceDomainObjectError::AllocationSize => {
                HYPER_NATIVE_STATUS_INTERNAL
            }
            crate::accounting::ResourceDomainObjectError::Object(error) => {
                status_from_object_creation_error(error)
            }
            crate::accounting::ResourceDomainObjectError::Resource(error) => {
                status_from_resource_error(error)
            }
        },
        crate::process::hierarchy::Error::Task(error) => status_from_task_object_error(error),
    }
}

pub(super) const fn status_from_memory_object_error(
    error: crate::mm::user_space::MemoryObjectError,
) -> HyperNativeStatus {
    match error {
        crate::mm::user_space::MemoryObjectError::AlreadyPublished => HYPER_NATIVE_STATUS_BAD_STATE,
        crate::mm::user_space::MemoryObjectError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        crate::mm::user_space::MemoryObjectError::Object(error) => {
            status_from_object_creation_error(error)
        }
        crate::mm::user_space::MemoryObjectError::Resource(error) => {
            status_from_resource_error(error)
        }
        crate::mm::user_space::MemoryObjectError::AddressSpace(error) => {
            status_from_logical_error(error)
        }
        crate::mm::user_space::MemoryObjectError::Vmo(error) => status_from_vmo_error(error),
        crate::mm::user_space::MemoryObjectError::WrongVariant => HYPER_NATIVE_STATUS_BAD_STATE,
    }
}

pub(super) const fn status_from_vmo_error(
    error: crate::mm::user_space::VmoError<crate::mm::user_space::KernelPageError, ResourceError>,
) -> HyperNativeStatus {
    match error {
        crate::mm::user_space::VmoError::Account(error) => status_from_resource_error(error),
        crate::mm::user_space::VmoError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        crate::mm::user_space::VmoError::Backend(error) => status_from_page_error(error),
        crate::mm::user_space::VmoError::Busy
        | crate::mm::user_space::VmoError::CowRequired
        | crate::mm::user_space::VmoError::BackingNotResident => HYPER_NATIVE_STATUS_BUSY,
        crate::mm::user_space::VmoError::BackingIo => HYPER_NATIVE_STATUS_IO_ERROR,
        crate::mm::user_space::VmoError::InvalidRange
        | crate::mm::user_space::VmoError::SizeOverflow => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    }
}

pub(super) fn status_from_memory_service_error(error: MemoryServiceError) -> HyperNativeStatus {
    match error {
        MemoryServiceError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        MemoryServiceError::Machine(error) => status_from_machine_error(error),
        MemoryServiceError::MemoryObject(error) => status_from_memory_object_error(error),
        MemoryServiceError::Process(error) => status_from_process_error(error),
        MemoryServiceError::Scheduler(error) => status_from_scheduler_error(error),
        MemoryServiceError::Vfs(error) => status_from_vfs_error(error),
    }
}

pub(super) fn status_from_vm_service_error(error: crate::vm::service::Error) -> HyperNativeStatus {
    match error {
        crate::vm::service::Error::NotSupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        crate::vm::service::Error::InvalidArgument => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        crate::vm::service::Error::BadHandle => HYPER_NATIVE_STATUS_BAD_HANDLE,
        crate::vm::service::Error::AccessDenied => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        crate::vm::service::Error::Busy => HYPER_NATIVE_STATUS_BUSY,
        crate::vm::service::Error::WouldBlock => HYPER_NATIVE_STATUS_WOULD_BLOCK,
        crate::vm::service::Error::BadState => HYPER_NATIVE_STATUS_BAD_STATE,
        crate::vm::service::Error::ResourceLimit => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        crate::vm::service::Error::NoMemory => HYPER_NATIVE_STATUS_NO_MEMORY,
        crate::vm::service::Error::Fault => HYPER_NATIVE_STATUS_FAULT,
        crate::vm::service::Error::Io => HYPER_NATIVE_STATUS_IO_ERROR,
        crate::vm::service::Error::Internal => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) const fn status_from_loader_error(
    error: crate::process::LoaderError,
) -> HyperNativeStatus {
    match error {
        crate::process::LoaderError::Address => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        crate::process::LoaderError::Elf(_) => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        crate::process::LoaderError::Image(_) => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        crate::process::LoaderError::Machine(error) => status_from_machine_error(error),
        crate::process::LoaderError::Resource(error) => status_from_resource_error(error),
        crate::process::LoaderError::UnsupportedMachine => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
    }
}

fn status_from_bootstrap_error(error: crate::process::bootstrap::Error) -> HyperNativeStatus {
    use crate::process::bootstrap::Error;
    match error {
        Error::Channel(error) => status_from_byte_channel_error(error),
        Error::Process(error) => status_from_process_error(error),
    }
}

pub(super) fn status_from_console_service_error(error: ConsoleServiceError) -> HyperNativeStatus {
    match error {
        ConsoleServiceError::Process(error) => status_from_process_error(error),
        ConsoleServiceError::Io(crate::device::console::IoError::WouldBlock) => {
            HYPER_NATIVE_STATUS_WOULD_BLOCK
        }
    }
}

pub(super) fn status_from_vfs_service_error(error: VfsServiceError) -> HyperNativeStatus {
    match error {
        VfsServiceError::FileLock(error) => {
            use crate::fs::vfs::locks::LockError;
            match error {
                LockError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
                LockError::Resource(error) => status_from_resource_error(error),
                LockError::Wait(error) => status_from_object_wait_error(error),
                LockError::WrongDomain => HYPER_NATIVE_STATUS_INTERNAL,
                LockError::Closed => HYPER_NATIVE_STATUS_BAD_STATE,
                LockError::Busy => HYPER_NATIVE_STATUS_BUSY,
                LockError::WouldBlock => HYPER_NATIVE_STATUS_WOULD_BLOCK,
                LockError::TimedOut => hyper::abi::native::HYPER_NATIVE_STATUS_TIMED_OUT,
                LockError::Cancelled => hyper::abi::native::HYPER_NATIVE_STATUS_CANCELLED,
            }
        }
        VfsServiceError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsServiceError::Process(error) => status_from_process_error(error),
        VfsServiceError::FileSystem(error) => status_from_vfs_error(error),
    }
}

pub(super) const fn status_from_vfs_error(error: VfsError) -> HyperNativeStatus {
    match error {
        VfsError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        VfsError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        VfsError::Backend(crate::fs::vfs::instance::Error::Remote(error)) => {
            use hyper_filesystem::protocol::Error as BackendError;
            match error {
                BackendError::NoSpace => hyper::abi::native::HYPER_NATIVE_STATUS_NO_SPACE,
                BackendError::ReadOnly => hyper::abi::native::HYPER_NATIVE_STATUS_READ_ONLY,
                BackendError::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
                BackendError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
                BackendError::ResourceLimit => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
                _ => hyper::abi::native::HYPER_NATIVE_STATUS_IO_ERROR,
            }
        }
        VfsError::Backend(_) => HYPER_NATIVE_STATUS_INTERNAL,
        VfsError::InvalidDirectoryCookie => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsError::InvalidPath => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsError::AlreadyExists => hyper::abi::native::HYPER_NATIVE_STATUS_ALREADY_EXISTS,
        VfsError::NotEmpty => hyper::abi::native::HYPER_NATIVE_STATUS_NOT_EMPTY,
        VfsError::InvalidSize => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsError::Missing => HYPER_NATIVE_STATUS_NOT_FOUND,
        VfsError::NotDirectory => hyper::abi::native::HYPER_NATIVE_STATUS_NOT_DIRECTORY,
        VfsError::NotRegularFile => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsError::IsDirectory => hyper::abi::native::HYPER_NATIVE_STATUS_IS_DIRECTORY,
        VfsError::SymlinkLoop => hyper::abi::native::HYPER_NATIVE_STATUS_SYMLINK_LOOP,
        VfsError::NotSymlink | VfsError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        VfsError::AccessDenied => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        VfsError::Busy => HYPER_NATIVE_STATUS_BUSY,
        VfsError::Unsupported => hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        VfsError::CrossDevice => hyper::abi::native::HYPER_NATIVE_STATUS_CROSS_DEVICE,
        VfsError::NotExecutable => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        VfsError::Object(error) => status_from_object_creation_error(error),
        VfsError::Resource(error) => status_from_resource_error(error),
    }
}

pub(super) const fn status_from_byte_channel_error(error: ByteChannelError) -> HyperNativeStatus {
    match error {
        ByteChannelError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ByteChannelError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        ByteChannelError::MessageTooLarge => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ByteChannelError::EndpointClosed => HYPER_NATIVE_STATUS_BAD_STATE,
        ByteChannelError::PeerClosed => HYPER_NATIVE_STATUS_PEER_CLOSED,
        ByteChannelError::WouldBlock => HYPER_NATIVE_STATUS_WOULD_BLOCK,
        ByteChannelError::Busy | ByteChannelError::StaleMessage => HYPER_NATIVE_STATUS_BUSY,
        ByteChannelError::SequenceExhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        ByteChannelError::Resource(error) => status_from_resource_error(error),
    }
}

pub(super) const fn status_from_event_error(error: EventError) -> HyperNativeStatus {
    match error {
        EventError::InvalidSignals => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        EventError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        EventError::Resource(error) => status_from_resource_error(error),
        EventError::SignalWait(error) => status_from_signal_wait_error(error),
    }
}

pub(super) const fn status_from_object_wait_error(error: ObjectWaitError) -> HyperNativeStatus {
    match error {
        ObjectWaitError::AllocationSize => HYPER_NATIVE_STATUS_INTERNAL,
        ObjectWaitError::Deadline(error) => status_from_deadline_error(error),
        ObjectWaitError::InvalidSignals => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        ObjectWaitError::Resource(error) => status_from_resource_error(error),
        ObjectWaitError::Signal(error) => status_from_signal_wait_error(error),
        ObjectWaitError::Timer(error) => status_from_timed_wait_error(error),
    }
}

pub(super) const fn status_from_deadline_error(error: crate::time::Error) -> HyperNativeStatus {
    match error {
        crate::time::Error::Conversion(_) | crate::time::Error::DeadlineTooFar => {
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT
        }
        _ => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) const fn status_from_timed_wait_error(error: TimedWaitError) -> HyperNativeStatus {
    match error {
        TimedWaitError::Allocation
        | TimedWaitError::Time(crate::time::Error::TimerQueue(
            crate::time::queue::TimerQueueError::Allocation,
        )) => HYPER_NATIVE_STATUS_NO_MEMORY,
        TimedWaitError::Scheduler(_)
        | TimedWaitError::Time(_)
        | TimedWaitError::TimerCleanup(_) => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) const fn status_from_signal_wait_error(error: SignalWaitError) -> HyperNativeStatus {
    match error {
        SignalWaitError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        SignalWaitError::SequenceExhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        SignalWaitError::Scheduler(_) => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) const fn status_from_handle_error(error: HandleError) -> HyperNativeStatus {
    match error {
        HandleError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        HandleError::InvalidHandle | HandleError::WrongObjectType => HYPER_NATIVE_STATUS_BAD_HANDLE,
        HandleError::Busy => HYPER_NATIVE_STATUS_BUSY,
        HandleError::AccessDenied => HYPER_NATIVE_STATUS_ACCESS_DENIED,
        HandleError::UnsupportedRights | HandleError::UnsupportedFlags => {
            HYPER_NATIVE_STATUS_INVALID_ARGUMENT
        }
        HandleError::UnsupportedTransfer => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        HandleError::ObjectRetired | HandleError::TableRetired => HYPER_NATIVE_STATUS_BAD_STATE,
        HandleError::ActiveHandleLimit
        | HandleError::ReservationIdExhausted
        | HandleError::ReservationTooLarge
        | HandleError::TableFull => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        HandleError::OutstandingReservation => HYPER_NATIVE_STATUS_BUSY,
        HandleError::ObjectAlreadyActive | HandleError::EmptyReservation => {
            HYPER_NATIVE_STATUS_INTERNAL
        }
    }
}

pub(super) const fn status_from_resource_error(error: ResourceError) -> HyperNativeStatus {
    match error {
        ResourceError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        ResourceError::HierarchyTooDeep
        | ResourceError::TooManyChargeDimensions
        | ResourceError::LimitExceeded { .. }
        | ResourceError::UsageOverflow { .. }
        | ResourceError::ChildCountExhausted
        | ResourceError::DomainIdExhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        ResourceError::DomainInactive(_)
        | ResourceError::OutstandingUsage
        | ResourceError::ActiveChildren
        | ResourceError::RetirementNotStarted => HYPER_NATIVE_STATUS_BAD_STATE,
        ResourceError::EmptyCharge | ResourceError::LimitBelowUsage { .. } => {
            HYPER_NATIVE_STATUS_INTERNAL
        }
    }
}

pub(super) const fn status_from_machine_error(error: MachineError) -> HyperNativeStatus {
    match error {
        MachineError::Allocation | MachineError::Page(_) => HYPER_NATIVE_STATUS_NO_MEMORY,
        MachineError::Address(error) => status_from_address_error(error),
        MachineError::Logical(error) => status_from_logical_error(error),
        MachineError::Resource(error) => status_from_resource_error(error),
        MachineError::Residency(_) | MachineError::Transport => HYPER_NATIVE_STATUS_BUSY,
        MachineError::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        MachineError::Hal(_) | MachineError::Identifier(_) | MachineError::Vmo(_) => {
            HYPER_NATIVE_STATUS_INTERNAL
        }
        MachineError::SizeOverflow => HYPER_NATIVE_STATUS_FAULT,
    }
}

pub(super) const fn status_from_address_error(_: AddressError) -> HyperNativeStatus {
    HYPER_NATIVE_STATUS_FAULT
}

pub(super) const fn status_from_logical_error(
    error: AddressSpaceError<crate::mm::user_space::KernelPageError, ResourceError>,
) -> HyperNativeStatus {
    match error {
        AddressSpaceError::Account(error) => status_from_resource_error(error),
        AddressSpaceError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
        AddressSpaceError::Stopped => HYPER_NATIVE_STATUS_BAD_STATE,
        AddressSpaceError::Busy | AddressSpaceError::CowRequired => HYPER_NATIVE_STATUS_BUSY,
        AddressSpaceError::Backend(error) => status_from_page_error(error),
        AddressSpaceError::BackingIo => HYPER_NATIVE_STATUS_IO_ERROR,
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
        | AddressSpaceError::WritableExecutableBacking => HYPER_NATIVE_STATUS_FAULT,
        AddressSpaceError::IdentityExhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
    }
}

pub(super) const fn status_from_page_error(
    error: crate::mm::user_space::KernelPageError,
) -> HyperNativeStatus {
    match error {
        crate::mm::user_space::KernelPageError::Allocation(_) => HYPER_NATIVE_STATUS_NO_MEMORY,
        crate::mm::user_space::KernelPageError::AddressOverflow
        | crate::mm::user_space::KernelPageError::Range => HYPER_NATIVE_STATUS_FAULT,
        crate::mm::user_space::KernelPageError::MissingLinearMap
        | crate::mm::user_space::KernelPageError::Unsupported => HYPER_NATIVE_STATUS_INTERNAL,
    }
}

pub(super) fn status_from_retention_error(
    error: crate::process::retention::Error,
) -> HyperNativeStatus {
    use crate::process::retention::Error;
    match error {
        Error::Process(error) => status_from_process_error(error),
        Error::Invalid => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        Error::Busy => HYPER_NATIVE_STATUS_WOULD_BLOCK,
        Error::Revoked => hyper::abi::native::HYPER_NATIVE_STATUS_REVOKED,
        Error::NotFound => HYPER_NATIVE_STATUS_NOT_FOUND,
        Error::Limit => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
    }
}
