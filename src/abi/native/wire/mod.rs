// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native wire-record decoding, validation, and byte encoding.

pub(super) mod memory_statistics;
mod object_details;
pub(super) use object_details::encode_object_details;

use crate::capability::handle_wire;

use alloc::vec::Vec;

use hyper::abi::native::{
    HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES, HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES,
    HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES,
    HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE, HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE,
    HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS, HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES,
    HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY, HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE,
    HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER, HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK,
    HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES, HYPER_NATIVE_OBJECT_BYTE_CHANNEL,
    HYPER_NATIVE_OBJECT_CAPABILITY_CHANNEL, HYPER_NATIVE_OBJECT_CONSOLE,
    HYPER_NATIVE_OBJECT_CPU_INSPECTOR, HYPER_NATIVE_OBJECT_DIRECTORY, HYPER_NATIVE_OBJECT_EVENT,
    HYPER_NATIVE_OBJECT_EXECUTABLE_AUTHORITY, HYPER_NATIVE_OBJECT_FILE,
    HYPER_NATIVE_OBJECT_HANDLE_STATE_ACTIVE, HYPER_NATIVE_OBJECT_HANDLE_STATE_RETIRED,
    HYPER_NATIVE_OBJECT_HANDLE_STATE_UNPUBLISHED, HYPER_NATIVE_OBJECT_MEMORY_INSPECTOR,
    HYPER_NATIVE_OBJECT_OBJECT_INSPECTOR, HYPER_NATIVE_OBJECT_PENDING_VIRTUAL_MACHINE,
    HYPER_NATIVE_OBJECT_PROCESS, HYPER_NATIVE_OBJECT_PROCESS_BUILDER,
    HYPER_NATIVE_OBJECT_RESOURCE_DOMAIN, HYPER_NATIVE_OBJECT_TASK_FACTORY,
    HYPER_NATIVE_OBJECT_TASK_GROUP, HYPER_NATIVE_OBJECT_TASK_INSPECTOR, HYPER_NATIVE_OBJECT_THREAD,
    HYPER_NATIVE_OBJECT_VIRTUAL_CPU, HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE,
    HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_AUTHORITY,
    HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_LEASE, HYPER_NATIVE_OBJECT_VIRTUAL_SERIAL,
    HYPER_NATIVE_OBJECT_VMAR, HYPER_NATIVE_OBJECT_VMO, HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS,
    HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS, HYPER_NATIVE_PROCESS_PHASE_CREATED,
    HYPER_NATIVE_PROCESS_PHASE_PREPARED, HYPER_NATIVE_PROCESS_PHASE_RETIRED,
    HYPER_NATIVE_PROCESS_PHASE_RETIRING, HYPER_NATIVE_PROCESS_PHASE_RUNNING,
    HYPER_NATIVE_PROCESS_PHASE_STOPPED, HYPER_NATIVE_PROCESS_PHASE_STOPPING,
    HYPER_NATIVE_PROCESS_TERMINAL_FAULT, HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED,
    HYPER_NATIVE_PROCESS_TERMINAL_NONE, HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED,
    HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED, HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP,
    HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED, HYPER_NATIVE_RESOURCE_LIMITS_MIN_SIZE,
    HYPER_NATIVE_STATUS_INTERNAL, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HYPER_NATIVE_STATUS_NO_MEMORY, HYPER_NATIVE_THREAD_REGISTRY_RESIDENT,
    HYPER_NATIVE_THREAD_REGISTRY_RETIRING, HYPER_NATIVE_THREAD_ROLE_BOOTSTRAP,
    HYPER_NATIVE_THREAD_ROLE_IDLE, HYPER_NATIVE_THREAD_ROLE_KERNEL, HYPER_NATIVE_THREAD_ROLE_USER,
    HYPER_NATIVE_THREAD_ROLE_VCPU, HYPER_NATIVE_VIRTUAL_CPU_BOOTSTRAP_MIN_SIZE,
    HYPER_NATIVE_VIRTUAL_MACHINE_CONFIGURATION_MIN_SIZE,
    HYPER_NATIVE_VIRTUAL_SERIAL_MAX_TRANSFER_BYTES, HyperNativeCapabilityDisposition,
    HyperNativeCapabilityReceiveSlot, HyperNativeCpuObservation, HyperNativeDirectoryEntry,
    HyperNativeDirectoryInfo, HyperNativeFileInfo, HyperNativeHandleInfo,
    HyperNativeHandleInspection, HyperNativeMemoryObservation, HyperNativeObjectBasicInfo,
    HyperNativeObjectInspection, HyperNativeObjectWaitItem, HyperNativeProcessInfo,
    HyperNativeResourceLimits, HyperNativeStatus, HyperNativeTaskProcess, HyperNativeTaskThread,
    HyperNativeVirtualCpuBootstrap, HyperNativeVirtualCpuInfo,
    HyperNativeVirtualMachineConfiguration, HyperNativeVirtualMachineInfo,
};

use {crate::capability::HandleInfo, crate::capability::HandleValue, crate::capability::Rights};
use {
    crate::diagnostics::inspect::HANDLE_PAGE_CAPACITY, crate::diagnostics::inspect::Page,
    crate::diagnostics::inspect::ProcessHandleSnapshot,
    crate::diagnostics::inspect::TaskThreadSnapshot,
};
use {crate::fs::vfs::DirectoryInfo, crate::fs::vfs::DirectoryPage, crate::fs::vfs::FileInfo};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};
use {
    crate::process::ProcessPhase, crate::process::ProcessSnapshot, crate::process::TerminalReason,
};

use super::Arguments;
use super::services::UserMemoryServices;
use super::status::{
    status_from_address_error, status_from_handle_error, status_from_process_error,
};

pub(super) const HANDLE_INFO_SIZE: usize = core::mem::size_of::<HyperNativeHandleInfo>();
pub(super) const OBJECT_BASIC_INFO_SIZE: usize = core::mem::size_of::<HyperNativeObjectBasicInfo>();
pub(super) const PROCESS_INFO_SIZE: usize = core::mem::size_of::<HyperNativeProcessInfo>();

pub(super) type ProcessBuilderHandleRequest = (
    HandleValue,
    u32,
    crate::object::ObjectKind,
    Option<Rights>,
    crate::capability::HandleTransferOperation,
);

pub(super) fn parse_u32(value: u64) -> Result<u32, HyperNativeStatus> {
    u32::try_from(value).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}

pub(super) fn parse_handle(raw: u64) -> Result<HandleValue, HyperNativeStatus> {
    handle_wire::from_abi(raw).map_err(status_from_handle_error)
}

pub(super) fn decode_resource_limits(
    services: &impl UserMemoryServices,
    arguments: &Arguments,
) -> Result<crate::accounting::ResourceLimits, HyperNativeStatus> {
    use crate::accounting::ResourceKind;

    type Record = HyperNativeResourceLimits;
    const SIZE: usize = core::mem::size_of::<Record>();
    require_zero(&arguments[2..])?;
    let record = copy_extensible_input_record::<SIZE>(
        services,
        arguments[0],
        arguments[1],
        HYPER_NATIVE_RESOURCE_LIMITS_MIN_SIZE,
    )?;
    if read_record_u64(&record, core::mem::offset_of!(Record, reserved)) != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(crate::accounting::ResourceLimits::UNLIMITED
        .with(
            ResourceKind::KernelMemoryBytes,
            read_record_u64(&record, core::mem::offset_of!(Record, kernel_memory_bytes)),
        )
        .with(
            ResourceKind::Processes,
            read_record_u64(&record, core::mem::offset_of!(Record, processes)),
        )
        .with(
            ResourceKind::Threads,
            read_record_u64(&record, core::mem::offset_of!(Record, threads)),
        )
        .with(
            ResourceKind::Handles,
            read_record_u64(&record, core::mem::offset_of!(Record, handles)),
        )
        .with(
            ResourceKind::KernelObjects,
            read_record_u64(&record, core::mem::offset_of!(Record, kernel_objects)),
        )
        .with(
            ResourceKind::CommittedPages,
            read_record_u64(&record, core::mem::offset_of!(Record, committed_pages)),
        )
        .with(
            ResourceKind::PinnedPages,
            read_record_u64(&record, core::mem::offset_of!(Record, pinned_pages)),
        )
        .with(
            ResourceKind::GuestPages,
            read_record_u64(&record, core::mem::offset_of!(Record, guest_pages)),
        )
        .with(
            ResourceKind::IpcMessages,
            read_record_u64(&record, core::mem::offset_of!(Record, ipc_messages)),
        )
        .with(
            ResourceKind::IpcBytes,
            read_record_u64(&record, core::mem::offset_of!(Record, ipc_bytes)),
        )
        .with(
            ResourceKind::IpcHandles,
            read_record_u64(&record, core::mem::offset_of!(Record, ipc_handles)),
        )
        .with(
            ResourceKind::Subscriptions,
            read_record_u64(&record, core::mem::offset_of!(Record, subscriptions)),
        )
        .with(
            ResourceKind::Timers,
            read_record_u64(&record, core::mem::offset_of!(Record, timers)),
        )
        .with(
            ResourceKind::VirtualMachines,
            read_record_u64(&record, core::mem::offset_of!(Record, virtual_machines)),
        )
        .with(
            ResourceKind::VirtualCpus,
            read_record_u64(&record, core::mem::offset_of!(Record, virtual_cpus)),
        )
        .with(
            ResourceKind::DeviceLeases,
            read_record_u64(&record, core::mem::offset_of!(Record, device_leases)),
        )
        .with(
            ResourceKind::DmaMappings,
            read_record_u64(&record, core::mem::offset_of!(Record, dma_mappings)),
        )
        .with(
            ResourceKind::UserAddressSpaces,
            read_record_u64(&record, core::mem::offset_of!(Record, user_address_spaces)),
        )
        .with(
            ResourceKind::UserMappings,
            read_record_u64(&record, core::mem::offset_of!(Record, user_mappings)),
        ))
}

pub(super) fn decode_virtual_machine_configuration(
    services: &impl UserMemoryServices,
    arguments: &Arguments,
) -> Result<crate::vm::objects::VirtualMachineConfiguration, HyperNativeStatus> {
    type Record = HyperNativeVirtualMachineConfiguration;
    const SIZE: usize = core::mem::size_of::<Record>();
    require_zero(&arguments[2..])?;
    let record = copy_extensible_input_record::<SIZE>(
        services,
        arguments[0],
        arguments[1],
        HYPER_NATIVE_VIRTUAL_MACHINE_CONFIGURATION_MIN_SIZE,
    )?;
    let flags = read_record_u32(&record, core::mem::offset_of!(Record, flags));
    if flags != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(crate::vm::objects::VirtualMachineConfiguration {
        guest_physical_base: read_record_u64(
            &record,
            core::mem::offset_of!(Record, guest_physical_base),
        ),
        memory_size: read_record_u64(&record, core::mem::offset_of!(Record, memory_size)),
        vcpu_count: read_record_u32(&record, core::mem::offset_of!(Record, vcpu_count)),
        architecture: read_record_u32(&record, core::mem::offset_of!(Record, architecture)),
        platform_profile: read_record_u32(&record, core::mem::offset_of!(Record, platform_profile)),
    })
}

pub(super) fn decode_virtual_cpu_bootstrap(
    services: &impl UserMemoryServices,
    arguments: &Arguments,
) -> Result<crate::vm::objects::VirtualCpuBootstrap, HyperNativeStatus> {
    type Record = HyperNativeVirtualCpuBootstrap;
    const SIZE: usize = core::mem::size_of::<Record>();
    require_zero(&arguments[2..])?;
    let record = copy_extensible_input_record::<SIZE>(
        services,
        arguments[0],
        arguments[1],
        HYPER_NATIVE_VIRTUAL_CPU_BOOTSTRAP_MIN_SIZE,
    )?;
    let vcpu_id = read_record_u32(&record, core::mem::offset_of!(Record, vcpu_id));
    let flags = read_record_u32(&record, core::mem::offset_of!(Record, flags));
    let reserved = read_record_u64(&record, core::mem::offset_of!(Record, reserved));
    if vcpu_id != 0 || flags != 0 || reserved != 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(crate::vm::objects::VirtualCpuBootstrap {
        entry: read_record_u64(&record, core::mem::offset_of!(Record, entry)),
        stack: read_record_u64(&record, core::mem::offset_of!(Record, stack)),
        arguments: [
            read_record_u64(&record, core::mem::offset_of!(Record, argument0)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument1)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument2)),
            read_record_u64(&record, core::mem::offset_of!(Record, argument3)),
        ],
    })
}

pub(super) fn read_record_u32(record: &[u8], offset: usize) -> u32 {
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&record[offset..offset + 4]);
    u32::from_ne_bytes(bytes)
}

pub(super) fn read_record_u64(record: &[u8], offset: usize) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&record[offset..offset + 8]);
    u64::from_ne_bytes(bytes)
}

pub(super) fn parse_wait_many(
    arguments: &Arguments,
) -> Result<(UserSlice, usize, u64), HyperNativeStatus> {
    if arguments[1] == 0 || arguments[1] > HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let item_count =
        usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let bytes = arguments[1]
        .checked_mul(core::mem::size_of::<HyperNativeObjectWaitItem>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let items =
        UserSlice::new(UserAddress::new(arguments[0]), bytes).map_err(status_from_address_error)?;
    Ok((items, item_count, arguments[2]))
}

pub(super) fn parse_byte_channel_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[0] != 0 || arguments[2] > HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[1], arguments[2])
}

pub(super) fn parse_capability_channel_send(
    arguments: &Arguments,
) -> Result<(Option<UserSlice>, Option<UserSlice>), HyperNativeStatus> {
    if arguments[0] != 0
        || arguments[2] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES
        || arguments[4] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let disposition_bytes = capability_disposition_bytes(arguments[4])?;
    Ok((
        optional_user_slice(arguments[1], arguments[2])?,
        optional_user_slice(arguments[3], disposition_bytes)?,
    ))
}

pub(super) fn parse_capability_channel_receive(
    arguments: &Arguments,
) -> Result<(u64, Option<UserSlice>, Option<UserSlice>), HyperNativeStatus> {
    if arguments[2] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES
        || arguments[4] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let slot_bytes = capability_receive_slot_bytes(arguments[4])?;
    Ok((
        arguments[0],
        optional_user_slice(arguments[1], arguments[2])?,
        optional_user_slice(arguments[3], slot_bytes)?,
    ))
}

pub(super) fn capability_disposition_bytes(record_count: u64) -> Result<u64, HyperNativeStatus> {
    capability_record_bytes::<HyperNativeCapabilityDisposition>(record_count)
}

pub(super) fn capability_receive_slot_bytes(record_count: u64) -> Result<u64, HyperNativeStatus> {
    capability_record_bytes::<HyperNativeCapabilityReceiveSlot>(record_count)
}

pub(super) fn capability_record_bytes<Record>(record_count: u64) -> Result<u64, HyperNativeStatus> {
    record_count
        .checked_mul(core::mem::size_of::<Record>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}

pub(super) fn parse_builder_create(
    arguments: &Arguments,
) -> Result<(HandleValue, HandleValue, HandleValue, HandleValue), HyperNativeStatus> {
    Ok((
        parse_handle(arguments[0])?,
        parse_handle(arguments[1])?,
        parse_handle(arguments[2])?,
        parse_handle(arguments[3])?,
    ))
}

pub(super) fn parse_builder_text(
    arguments: &Arguments,
    maximum_bytes: u64,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[1] > maximum_bytes {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[0], arguments[1])
}

pub(super) fn parse_affinity_words(
    pointer: u64,
    count: u64,
) -> Result<(Option<UserSlice>, usize), HyperNativeStatus> {
    if pointer == 0 || count == 0 || count > HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let bytes = count
        .checked_mul(core::mem::size_of::<u64>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let word_count = usize::try_from(count).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    Ok((optional_user_slice(pointer, bytes)?, word_count))
}

pub(super) fn parse_affinity_request(
    arguments: &Arguments,
) -> Result<(Option<UserSlice>, usize), HyperNativeStatus> {
    if arguments[2..].iter().any(|value| *value != 0) {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    parse_affinity_words(arguments[0], arguments[1])
}

pub(super) fn parse_builder_handle(
    arguments: &Arguments,
) -> Result<ProcessBuilderHandleRequest, HyperNativeStatus> {
    let purpose = u32::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let raw_kind = u32::try_from(arguments[2]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let expected_kind = parse_object_kind(raw_kind)?;
    let requested_rights = if arguments[3] == HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS {
        None
    } else {
        Some(Rights::from_bits(arguments[3]).ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?)
    };
    let operation = match arguments[4] {
        HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE => {
            crate::capability::HandleTransferOperation::Move
        }
        HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE => {
            crate::capability::HandleTransferOperation::Copy
        }
        _ => return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
    };
    Ok((
        parse_handle(arguments[0])?,
        purpose,
        expected_kind,
        requested_rights,
        operation,
    ))
}

pub(super) fn parse_object_kind(raw: u32) -> Result<crate::object::ObjectKind, HyperNativeStatus> {
    use crate::object::ObjectKind;

    match raw {
        hyper::abi::native::HYPER_NATIVE_OBJECT_BLOCK_QUEUE => Ok(ObjectKind::BLOCK_QUEUE),
        HYPER_NATIVE_OBJECT_EVENT => Ok(ObjectKind::EVENT),
        HYPER_NATIVE_OBJECT_BYTE_CHANNEL => Ok(ObjectKind::BYTE_CHANNEL),
        HYPER_NATIVE_OBJECT_CAPABILITY_CHANNEL => Ok(ObjectKind::CAPABILITY_CHANNEL),
        HYPER_NATIVE_OBJECT_THREAD => Ok(ObjectKind::THREAD),
        HYPER_NATIVE_OBJECT_PROCESS => Ok(ObjectKind::PROCESS),
        hyper::abi::native::HYPER_NATIVE_OBJECT_PROCESS_RETIREMENT => {
            Ok(ObjectKind::PROCESS_RETIREMENT)
        }
        hyper::abi::native::HYPER_NATIVE_OBJECT_CRASH_VIEW => Ok(ObjectKind::CRASH_VIEW),
        HYPER_NATIVE_OBJECT_TASK_GROUP => Ok(ObjectKind::TASK_GROUP),
        HYPER_NATIVE_OBJECT_RESOURCE_DOMAIN => Ok(ObjectKind::RESOURCE_DOMAIN),
        HYPER_NATIVE_OBJECT_TASK_FACTORY => Ok(ObjectKind::TASK_FACTORY),
        HYPER_NATIVE_OBJECT_EXECUTABLE_AUTHORITY => Ok(ObjectKind::EXECUTABLE_AUTHORITY),
        HYPER_NATIVE_OBJECT_VMO => Ok(ObjectKind::VMO),
        HYPER_NATIVE_OBJECT_VMAR => Ok(ObjectKind::VMAR),
        HYPER_NATIVE_OBJECT_CONSOLE => Ok(ObjectKind::CONSOLE),
        hyper::abi::native::HYPER_NATIVE_OBJECT_DIRECTORY_STREAM => {
            Ok(ObjectKind::DIRECTORY_STREAM)
        }
        HYPER_NATIVE_OBJECT_DIRECTORY => Ok(ObjectKind::DIRECTORY),
        HYPER_NATIVE_OBJECT_FILE => Ok(ObjectKind::FILE),
        HYPER_NATIVE_OBJECT_PROCESS_BUILDER => Ok(ObjectKind::PROCESS_BUILDER),
        HYPER_NATIVE_OBJECT_TASK_INSPECTOR => Ok(ObjectKind::TASK_INSPECTOR),
        HYPER_NATIVE_OBJECT_OBJECT_INSPECTOR => Ok(ObjectKind::OBJECT_INSPECTOR),
        HYPER_NATIVE_OBJECT_MEMORY_INSPECTOR => Ok(ObjectKind::MEMORY_INSPECTOR),
        HYPER_NATIVE_OBJECT_CPU_INSPECTOR => Ok(ObjectKind::CPU_INSPECTOR),
        HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_AUTHORITY => {
            Ok(ObjectKind::VIRTUAL_MACHINE_CREATION_AUTHORITY)
        }
        HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_LEASE => {
            Ok(ObjectKind::VIRTUAL_MACHINE_CREATION_LEASE)
        }
        HYPER_NATIVE_OBJECT_PENDING_VIRTUAL_MACHINE => Ok(ObjectKind::PENDING_VIRTUAL_MACHINE),
        HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE => Ok(ObjectKind::VIRTUAL_MACHINE),
        hyper::abi::native::HYPER_NATIVE_OBJECT_DEVICE_ASSIGNMENT_AUTHORITY => {
            Ok(ObjectKind::DEVICE_ASSIGNMENT_AUTHORITY)
        }
        hyper::abi::native::HYPER_NATIVE_OBJECT_PHYSICAL_DEVICE => Ok(ObjectKind::PHYSICAL_DEVICE),
        hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_MAILBOX => Ok(ObjectKind::GUEST_MAILBOX),
        hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_NOTIFICATION => {
            Ok(ObjectKind::GUEST_NOTIFICATION)
        }
        hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_MEMORY => Ok(ObjectKind::GUEST_MEMORY),
        hyper::abi::native::HYPER_NATIVE_OBJECT_NATIVE_BLOCK => Ok(ObjectKind::NATIVE_BLOCK),
        HYPER_NATIVE_OBJECT_VIRTUAL_CPU => Ok(ObjectKind::VIRTUAL_CPU),
        HYPER_NATIVE_OBJECT_VIRTUAL_SERIAL => Ok(ObjectKind::VIRTUAL_SERIAL),
        hyper::abi::native::HYPER_NATIVE_OBJECT_EXECUTION_CONTEXT => {
            Ok(ObjectKind::EXECUTION_CONTEXT)
        }
        hyper::abi::native::HYPER_NATIVE_OBJECT_OBJECT_FACTORY => Ok(ObjectKind::OBJECT_FACTORY),
        hyper::abi::native::HYPER_NATIVE_OBJECT_SYSTEM_INFO => Ok(ObjectKind::SYSTEM_INFO),
        _ => Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
    }
}

pub(super) fn parse_console_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[0] != 0 || arguments[2] > HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[1], arguments[2])
}

pub(super) fn parse_virtual_serial_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[1] > HYPER_NATIVE_VIRTUAL_SERIAL_MAX_TRANSFER_BYTES
        || arguments[2..].iter().any(|argument| *argument != 0)
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[0], arguments[1])
}

pub(super) fn optional_user_slice(
    raw_address: u64,
    length: u64,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if length == 0 {
        return Ok(None);
    }
    UserSlice::new(UserAddress::new(raw_address), length)
        .map(Some)
        .map_err(status_from_address_error)
}

pub(super) fn require_zero(arguments: &[u64]) -> Result<(), HyperNativeStatus> {
    if arguments.iter().all(|argument| *argument == 0) {
        Ok(())
    } else {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    }
}

#[derive(Clone, Copy)]
pub(super) struct InfoRequest {
    pub(super) destination: UserSlice,
    pub(super) supported_size: usize,
}

pub(super) fn prepare_info_request(
    arguments: &Arguments,
    minimum_size: usize,
    supported_size: usize,
) -> Result<InfoRequest, HyperNativeStatus> {
    require_zero(&arguments[2..])?;
    let capacity =
        usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let maximum = HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES as usize;
    if minimum_size == 0
        || minimum_size > supported_size
        || capacity < minimum_size
        || capacity > maximum
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let write_size = capacity.min(supported_size);
    let write_size = u64::try_from(write_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination = UserSlice::new(UserAddress::new(arguments[0]), write_size)
        .map_err(status_from_address_error)?;
    Ok(InfoRequest {
        destination,
        supported_size,
    })
}

pub(super) fn copy_info_record(
    services: &impl UserMemoryServices,
    request: InfoRequest,
    record: &[u8],
) -> Result<u64, HyperNativeStatus> {
    let write_size =
        usize::try_from(request.destination.length()).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    if record.len() != request.supported_size || write_size > record.len() {
        return Err(HYPER_NATIVE_STATUS_INTERNAL);
    }
    services
        .copy_to_user(request.destination, &record[..write_size])
        .map_err(status_from_process_error)?;
    u64::try_from(request.supported_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)
}

pub(super) fn copy_extensible_input_record<const SIZE: usize>(
    services: &impl UserMemoryServices,
    address: u64,
    requested_size: u64,
    minimum_size: usize,
) -> Result<[u8; SIZE], HyperNativeStatus> {
    const EXTENSION_CHUNK_BYTES: usize = 64;

    let requested =
        usize::try_from(requested_size).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let maximum = HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES as usize;
    if minimum_size == 0 || minimum_size > SIZE || requested < minimum_size || requested > maximum {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let base = UserAddress::new(address);
    let prefix_size = requested.min(SIZE);
    let prefix = UserSlice::new(
        base,
        u64::try_from(prefix_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
    )
    .map_err(status_from_address_error)?;
    let mut record = [0_u8; SIZE];
    services
        .copy_from_user(prefix, &mut record[..prefix_size])
        .map_err(status_from_process_error)?;

    let mut extension = [0_u8; EXTENSION_CHUNK_BYTES];
    let mut offset = SIZE;
    while offset < requested {
        let length = (requested - offset).min(extension.len());
        let raw_offset = u64::try_from(offset).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        let extension_base = base
            .checked_add(raw_offset)
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let source = UserSlice::new(
            extension_base,
            u64::try_from(length).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
        )
        .map_err(status_from_address_error)?;
        services
            .copy_from_user(source, &mut extension[..length])
            .map_err(status_from_process_error)?;
        if extension[..length].iter().any(|byte| *byte != 0) {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        offset += length;
    }
    Ok(record)
}

pub(super) fn parse_inspector_scan(
    arguments: &Arguments,
    capacity: usize,
    record_size: usize,
) -> Result<(u64, UserSlice), HyperNativeStatus> {
    let requested =
        usize::try_from(arguments[2]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    if requested != capacity {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let bytes = capacity
        .checked_mul(record_size)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination =
        UserSlice::new(UserAddress::new(arguments[1]), bytes).map_err(status_from_address_error)?;
    Ok((arguments[0], destination))
}

pub(super) fn parse_handle_inspector_scan(
    arguments: &Arguments,
) -> Result<(u64, u64, UserSlice), HyperNativeStatus> {
    let requested =
        usize::try_from(arguments[3]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    if requested != HANDLE_PAGE_CAPACITY || arguments[0] == 0 {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let bytes = HANDLE_PAGE_CAPACITY
        .checked_mul(core::mem::size_of::<HyperNativeHandleInspection>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination =
        UserSlice::new(UserAddress::new(arguments[2]), bytes).map_err(status_from_address_error)?;
    Ok((arguments[0], arguments[1], destination))
}

pub(super) fn copy_encoded_page<T: Copy, const N: usize, const R: usize>(
    services: &impl UserMemoryServices,
    destination: UserSlice,
    page: &Page<T, N>,
    encode: impl Fn(T) -> [u8; R],
) -> Result<(usize, u64), HyperNativeStatus> {
    let byte_count = page
        .len()
        .checked_mul(R)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_count)
        .map_err(|_| HYPER_NATIVE_STATUS_NO_MEMORY)?;
    for entry in page.entries() {
        bytes.extend_from_slice(&encode(*entry));
    }
    let output = UserSlice::new(
        destination.base(),
        u64::try_from(byte_count).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
    )
    .map_err(status_from_address_error)?;
    services
        .copy_to_user(output, &bytes)
        .map_err(status_from_process_error)?;
    Ok((page.len(), page.next()))
}

// Encoding/copyout follows backend completion. Keep its record scratch out of
// the syscall frame while directory lookup may block deep in a filesystem.
#[inline(never)]
pub(super) fn copy_directory_page(
    services: &impl UserMemoryServices,
    destination: UserSlice,
    page: &DirectoryPage,
) -> Result<(usize, u64), HyperNativeStatus> {
    const RECORD_SIZE: usize = core::mem::size_of::<HyperNativeDirectoryEntry>();

    let byte_count = page
        .len()
        .checked_mul(RECORD_SIZE)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_count)
        .map_err(|_| HYPER_NATIVE_STATUS_NO_MEMORY)?;
    for entry in page.entries() {
        bytes.extend_from_slice(&encode_directory_entry(*entry)?);
    }
    let output = UserSlice::new(
        destination.base(),
        u64::try_from(byte_count).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
    )
    .map_err(status_from_address_error)?;
    services
        .copy_to_user(output, &bytes)
        .map_err(status_from_process_error)?;
    Ok((page.len(), page.next_cookie()))
}

pub(super) fn encode_directory_entry(
    snapshot: crate::fs::vfs::DirectoryEntrySnapshot,
) -> Result<[u8; core::mem::size_of::<HyperNativeDirectoryEntry>()], HyperNativeStatus> {
    const SIZE: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, size);
    const MODE: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, mode);
    const KIND: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, kind);
    const NAME_LENGTH: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, name_length);
    const NAME: usize = core::mem::offset_of!(HyperNativeDirectoryEntry, name);

    let mut record = [0_u8; core::mem::size_of::<HyperNativeDirectoryEntry>()];
    write_u64(&mut record, SIZE, snapshot.attributes.size());
    write_u32(&mut record, MODE, snapshot.attributes.mode());
    write_u32(
        &mut record,
        KIND,
        directory_entry_kind(snapshot.attributes.kind()),
    );
    write_u32(&mut record, NAME_LENGTH, snapshot.name_length);
    let name_length =
        usize::try_from(snapshot.name_length).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    let source = snapshot
        .name
        .get(..name_length)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let end = NAME
        .checked_add(name_length)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination = record
        .get_mut(NAME..end)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    destination.copy_from_slice(source);
    Ok(record)
}

pub(super) const fn directory_entry_kind(kind: crate::fs::primitives::NodeKind) -> u32 {
    match kind {
        crate::fs::primitives::NodeKind::File => HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE as u32,
        crate::fs::primitives::NodeKind::Directory => {
            HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY as u32
        }
        crate::fs::primitives::NodeKind::Symlink => {
            HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK as u32
        }
        crate::fs::primitives::NodeKind::Other => HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER as u32,
    }
}

pub(super) fn encode_file_info(
    info: FileInfo,
) -> [u8; core::mem::size_of::<HyperNativeFileInfo>()] {
    encode_file_info_fields(
        info.location.filesystem_id,
        info.location.mount_id,
        info.location.node_id,
        info.size,
        info.mode,
    )
}

pub(super) fn encode_file_info_fields(
    filesystem_id: u64,
    mount_id: u64,
    node_id: u64,
    size: u64,
    mode: u32,
) -> [u8; core::mem::size_of::<HyperNativeFileInfo>()] {
    let mut record = [0_u8; core::mem::size_of::<HyperNativeFileInfo>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, filesystem_id),
        filesystem_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, mount_id),
        mount_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, node_id),
        node_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, size),
        size,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeFileInfo, mode),
        mode,
    );
    record
}

pub(super) fn encode_directory_info(
    info: DirectoryInfo,
) -> [u8; core::mem::size_of::<HyperNativeDirectoryInfo>()] {
    encode_directory_info_fields(
        info.location.filesystem_id,
        info.location.mount_id,
        info.location.node_id,
        info.mode,
    )
}

pub(super) fn encode_virtual_machine_info(
    configuration: crate::vm::objects::VirtualMachineConfiguration,
    snapshot: crate::vm::objects::VirtualMachineSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeVirtualMachineInfo>()] {
    type Record = HyperNativeVirtualMachineInfo;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, resident_memory_bytes),
        snapshot.resident_memory_bytes.unwrap_or(u64::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        snapshot.phase,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu_count),
        configuration.vcpu_count,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, guest_physical_base),
        configuration.guest_physical_base,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, memory_size),
        configuration.memory_size,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, architecture),
        configuration.architecture,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, platform_profile),
        configuration.platform_profile,
    );
    record
}

pub(super) fn encode_virtual_cpu_info(
    snapshot: crate::vm::objects::VirtualCpuSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeVirtualCpuInfo>()] {
    type Record = HyperNativeVirtualCpuInfo;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, host_cpu),
        snapshot.host_cpu.unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, migration_target),
        snapshot.migration_target.unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu_id),
        snapshot.id,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        snapshot.phase,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, scheduler_thread_id),
        snapshot.thread,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, terminal_reason),
        snapshot.terminal_reason,
    );
    record
}

pub(super) fn encode_directory_info_fields(
    filesystem_id: u64,
    mount_id: u64,
    node_id: u64,
    mode: u32,
) -> [u8; core::mem::size_of::<HyperNativeDirectoryInfo>()] {
    let mut record = [0_u8; core::mem::size_of::<HyperNativeDirectoryInfo>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, filesystem_id),
        filesystem_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, mount_id),
        mount_id,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, node_id),
        node_id,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeDirectoryInfo, mode),
        mode,
    );
    record
}

pub(super) fn encode_handle_info(info: HandleInfo) -> [u8; HANDLE_INFO_SIZE] {
    encode_handle_info_fields(info.kind.get(), info.flags.bits(), info.rights.bits())
}

pub(super) fn encode_handle_info_fields(
    object_kind: u32,
    flags: u32,
    rights: u64,
) -> [u8; HANDLE_INFO_SIZE] {
    let mut record = [0_u8; HANDLE_INFO_SIZE];
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeHandleInfo, object_kind),
        object_kind,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeHandleInfo, flags),
        flags,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeHandleInfo, rights),
        rights,
    );
    record
}

pub(super) fn encode_object_basic_info(info: HandleInfo) -> [u8; OBJECT_BASIC_INFO_SIZE] {
    encode_object_basic_info_fields(info.koid.get(), info.kind.get())
}

pub(super) fn encode_object_basic_info_fields(
    koid: u64,
    object_kind: u32,
) -> [u8; OBJECT_BASIC_INFO_SIZE] {
    let mut record = [0_u8; OBJECT_BASIC_INFO_SIZE];
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeObjectBasicInfo, koid),
        koid,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeObjectBasicInfo, object_kind),
        object_kind,
    );
    // The generated record's remaining bytes are the reserved-zero field.
    record
}

pub(super) fn encode_process_info(snapshot: ProcessSnapshot) -> [u8; PROCESS_INFO_SIZE] {
    let (reason, detail0, detail1) = match snapshot.terminal {
        None => (HYPER_NATIVE_PROCESS_TERMINAL_NONE, 0, 0),
        Some(TerminalReason::Requested) => (HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED, 0, 0),
        Some(TerminalReason::ThreadExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::ProcessExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::LastThreadExited { status }) => (
            HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED,
            status as u64,
            0,
        ),
        Some(TerminalReason::Fault { class, code }) => {
            (HYPER_NATIVE_PROCESS_TERMINAL_FAULT, u64::from(class), code)
        }
        Some(TerminalReason::TaskGroupStop { generation }) => {
            (HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP, generation, 0)
        }
    };
    encode_process_info_fields(
        process_phase(snapshot.phase),
        reason as u32,
        detail0,
        detail1,
    )
}

pub(super) fn encode_process_info_fields(
    phase: u32,
    reason: u32,
    detail0: u64,
    detail1: u64,
) -> [u8; PROCESS_INFO_SIZE] {
    let mut record = [0_u8; PROCESS_INFO_SIZE];
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, phase),
        phase,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, terminal_reason),
        reason,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, detail0),
        detail0,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(HyperNativeProcessInfo, detail1),
        detail1,
    );
    record
}

pub(super) fn encode_task_process(
    snapshot: ProcessSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeTaskProcess>()] {
    type Record = HyperNativeTaskProcess;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, phase),
        process_phase(snapshot.phase),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, terminal_reason),
        terminal_reason(snapshot.terminal),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, pending_threads),
        u32::try_from(snapshot.pending_threads).unwrap_or(u32::MAX),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, active_threads),
        u32::try_from(snapshot.active_threads).unwrap_or(u32::MAX),
    );
    let name = snapshot.name.as_bytes();
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, name_length),
        name.len() as u32,
    );
    let name_offset = core::mem::offset_of!(Record, name);
    record[name_offset..name_offset + name.len()].copy_from_slice(name);
    record
}

pub(super) fn encode_task_thread(
    snapshot: TaskThreadSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeTaskThread>()] {
    type Record = HyperNativeTaskThread;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, process_koid),
        snapshot.process_koid.map_or(0, crate::object::Koid::get),
    );
    let role = match snapshot.role {
        crate::task::ThreadRole::Bootstrap => HYPER_NATIVE_THREAD_ROLE_BOOTSTRAP,
        crate::task::ThreadRole::Idle => HYPER_NATIVE_THREAD_ROLE_IDLE,
        crate::task::ThreadRole::Kernel => HYPER_NATIVE_THREAD_ROLE_KERNEL,
        crate::task::ThreadRole::User => HYPER_NATIVE_THREAD_ROLE_USER,
        crate::task::ThreadRole::Vcpu => HYPER_NATIVE_THREAD_ROLE_VCPU,
    };
    let registry_phase = match snapshot.registry_phase {
        crate::task::ThreadObjectRegistryPhase::Resident => HYPER_NATIVE_THREAD_REGISTRY_RESIDENT,
        crate::task::ThreadObjectRegistryPhase::Retiring => HYPER_NATIVE_THREAD_REGISTRY_RETIRING,
    };
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, role),
        role as u32,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, registry_phase),
        registry_phase as u32,
    );
    let name = snapshot.name.as_bytes();
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, name_length),
        name.len() as u32,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, runtime_ns),
        snapshot.runtime_ns,
    );
    let name_offset = core::mem::offset_of!(Record, name);
    record[name_offset..name_offset + name.len()].copy_from_slice(name);
    record
}

pub(super) fn encode_memory_observation(
    snapshot: crate::diagnostics::inspect::MemoryObservation,
) -> [u8; core::mem::size_of::<HyperNativeMemoryObservation>()] {
    type Record = HyperNativeMemoryObservation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    macro_rules! field {
        ($name:ident) => {
            write_u64(
                &mut record,
                core::mem::offset_of!(Record, $name),
                snapshot.$name,
            );
        };
    }
    field!(captured_at_ns);
    field!(page_size);
    field!(total_bytes);
    field!(managed_bytes);
    field!(free_bytes);
    field!(kernel_bytes);
    field!(slab_bytes);
    field!(large_heap_bytes);
    field!(page_table_bytes);
    field!(vmo_bytes);
    field!(guest_loaned_bytes);
    field!(guest_private_bytes);
    field!(file_cache_bytes);
    field!(slab_reclaimable_bytes);
    field!(slab_sample_complete);
    field!(file_cache_reclaimable_bytes);
    field!(file_cache_sample_complete);
    field!(ramfs_capacity_bytes);
    record
}

pub(super) fn encode_cpu_observation(
    snapshot: crate::task::scheduler::CpuTimeSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeCpuObservation>()] {
    type Record = HyperNativeCpuObservation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    macro_rules! field {
        ($name:ident) => {
            write_u64(
                &mut record,
                core::mem::offset_of!(Record, $name),
                snapshot.$name,
            );
        };
    }
    field!(captured_at_ns);
    field!(reserved_clock);
    field!(online_cpus);
    field!(idle_ns);
    field!(kernel_thread_ns);
    field!(user_thread_ns);
    field!(vcpu_ns);
    write_u64(&mut record, core::mem::offset_of!(Record, reserved), 0);
    record
}

pub(super) fn encode_object_inspection(
    snapshot: crate::object::ObjectSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeObjectInspection>()] {
    type Record = HyperNativeObjectInspection;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, koid),
        snapshot.koid.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, object_kind),
        snapshot.kind.get(),
    );
    let (state, active) = match snapshot.handles {
        crate::object::ObjectHandleState::Unpublished => {
            (HYPER_NATIVE_OBJECT_HANDLE_STATE_UNPUBLISHED, 0)
        }
        crate::object::ObjectHandleState::Active(count) => (
            HYPER_NATIVE_OBJECT_HANDLE_STATE_ACTIVE,
            u64::try_from(count).unwrap_or(u64::MAX),
        ),
        crate::object::ObjectHandleState::Retired => (HYPER_NATIVE_OBJECT_HANDLE_STATE_RETIRED, 0),
    };
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, handle_state),
        state as u32,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, active_handles),
        active,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, supported_rights),
        snapshot.supported_rights.bits(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, strong_references),
        u64::try_from(snapshot.strong_references).unwrap_or(u64::MAX),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, kernel_service_references),
        usize_to_u64(snapshot.references.kernel_service),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, vm_device_binding_references),
        usize_to_u64(snapshot.references.vm_device_binding),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, scheduler_references),
        usize_to_u64(snapshot.references.scheduler),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, operation_references),
        usize_to_u64(snapshot.references.operation_pin),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, user_authority_references),
        usize_to_u64(snapshot.references.user_authority),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, publication_references),
        usize_to_u64(snapshot.references.publication),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, diagnostic_references),
        usize_to_u64(snapshot.references.diagnostic),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, retirement_references),
        usize_to_u64(snapshot.references.retirement),
    );
    record
}

pub(super) fn encode_handle_inspection(
    snapshot: ProcessHandleSnapshot,
) -> [u8; core::mem::size_of::<HyperNativeHandleInspection>()] {
    type Record = HyperNativeHandleInspection;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, process_koid),
        snapshot.process_koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, handle),
        handle_wire::to_abi(snapshot.handle.value),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, object_koid),
        snapshot.handle.info.koid.get(),
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, rights),
        snapshot.handle.info.rights.bits(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, object_kind),
        snapshot.handle.info.kind.get(),
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, flags),
        snapshot.handle.info.flags.bits(),
    );
    record
}

pub(super) fn terminal_reason(reason: Option<TerminalReason>) -> u32 {
    match reason {
        None => HYPER_NATIVE_PROCESS_TERMINAL_NONE as u32,
        Some(TerminalReason::Requested) => HYPER_NATIVE_PROCESS_TERMINAL_REQUESTED as u32,
        Some(TerminalReason::ThreadExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_THREAD_EXITED as u32
        }
        Some(TerminalReason::ProcessExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_PROCESS_EXITED as u32
        }
        Some(TerminalReason::LastThreadExited { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_LAST_THREAD_EXITED as u32
        }
        Some(TerminalReason::Fault { .. }) => HYPER_NATIVE_PROCESS_TERMINAL_FAULT as u32,
        Some(TerminalReason::TaskGroupStop { .. }) => {
            HYPER_NATIVE_PROCESS_TERMINAL_TASK_GROUP_STOP as u32
        }
    }
}

pub(super) fn write_u32(record: &mut [u8], offset: usize, value: u32) {
    record[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

pub(super) fn write_u64(record: &mut [u8], offset: usize, value: u64) {
    record[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

pub(super) fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

pub(super) const fn process_phase(phase: ProcessPhase) -> u32 {
    match phase {
        ProcessPhase::Prepared => HYPER_NATIVE_PROCESS_PHASE_PREPARED as u32,
        ProcessPhase::Created => HYPER_NATIVE_PROCESS_PHASE_CREATED as u32,
        ProcessPhase::Running => HYPER_NATIVE_PROCESS_PHASE_RUNNING as u32,
        ProcessPhase::Stopping => HYPER_NATIVE_PROCESS_PHASE_STOPPING as u32,
        ProcessPhase::Stopped => HYPER_NATIVE_PROCESS_PHASE_STOPPED as u32,
        ProcessPhase::Retiring => HYPER_NATIVE_PROCESS_PHASE_RETIRING as u32,
        ProcessPhase::Retired => HYPER_NATIVE_PROCESS_PHASE_RETIRED as u32,
    }
}

pub(super) fn encode_virtual_machine_platform_info(
    info: crate::vm::service::VirtualMachinePlatformInfo,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualMachinePlatformInfo>()] {
    type Record = hyper::abi::native::HyperNativeVirtualMachinePlatformInfo;
    let mut bytes = [0; core::mem::size_of::<Record>()];
    write_u32(
        &mut bytes,
        core::mem::offset_of!(Record, architecture),
        info.architecture,
    );
    write_u32(
        &mut bytes,
        core::mem::offset_of!(Record, platform_profile),
        info.platform_profile,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, counter_frequency_hz),
        info.counter_frequency_hz,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, riscv_isa),
        info.riscv_isa,
    );
    write_u64(
        &mut bytes,
        core::mem::offset_of!(Record, aarch64_gic_version),
        info.aarch64_gic_version,
    );
    bytes
}

pub(super) fn encode_virtual_machine_power_request(
    request: hyper_vm_models::arm::psci::Request,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualMachinePowerRequest>()] {
    use hyper::abi::native::{
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_OFF, HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_ON,
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_OFF,
        HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_RESET,
        HyperNativeVirtualMachinePowerRequest as Record,
    };
    use hyper_vm_models::arm::psci::Operation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    let operation = match request.operation {
        Operation::CpuOn => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_ON,
        Operation::CpuOff => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_CPU_OFF,
        Operation::SystemOff => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_OFF,
        Operation::SystemReset => HYPER_NATIVE_VIRTUAL_MACHINE_POWER_SYSTEM_RESET,
    };
    write_u64(&mut record, core::mem::offset_of!(Record, id), request.id);
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, vcpu),
        request.vcpu,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, operation),
        operation as u32,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, target),
        request.target,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, entry),
        request.entry,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, context),
        request.context,
    );
    record
}

pub(super) fn encode_virtual_cpu_mmio_request(
    request: hyper_vm_models::device::mmio::Request,
) -> [u8; core::mem::size_of::<hyper::abi::native::HyperNativeVirtualCpuMmioRequest>()] {
    use hyper::abi::native::HyperNativeVirtualCpuMmioRequest as Record;
    use hyper_vm_models::exit::MmioOperation;
    let mut record = [0_u8; core::mem::size_of::<Record>()];
    let (operation, value) = match request.access.operation() {
        MmioOperation::Read => (0, 0),
        MmioOperation::Write(value) => (1, value),
    };
    write_u64(&mut record, core::mem::offset_of!(Record, id), request.id);
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, device),
        request.device,
    );
    write_u64(
        &mut record,
        core::mem::offset_of!(Record, address),
        request.access.address().get(),
    );
    write_u64(&mut record, core::mem::offset_of!(Record, value), value);
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, operation),
        operation,
    );
    write_u32(
        &mut record,
        core::mem::offset_of!(Record, width),
        request.access.size() as u32,
    );
    record
}
