// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Resource limit record validation.

use super::{copy_extensible_input_record, read_record_u64, require_zero};
use crate::abi::native::Arguments;
use crate::abi::native::services::UserMemoryServices;
use hyper::abi::native::{
    HYPER_NATIVE_RESOURCE_LIMITS_MIN_SIZE, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HyperNativeResourceLimits, HyperNativeStatus,
};

pub(in crate::abi::native) fn decode_resource_limits(
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
