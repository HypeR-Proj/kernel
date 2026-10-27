// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native memory syscall validation.

use crate::capability::handle_wire;

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use crate::mm::user_space::UserSlice;
use hyper::abi::native::{
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_VMO_MAX_TRANSFER_BYTES, HyperNativeStatus,
};
use {crate::abi::native::services::MemoryServices, crate::abi::native::services::NativeAction};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::status_from_memory_service_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::optional_user_slice, crate::abi::native::wire::parse_handle,
    crate::abi::native::wire::require_zero,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_create(
    services: &impl MemoryServices,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[1..].iter().any(|value| *value != 0) {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        services
            .create_vmo(arguments[0])
            .map_err(status_from_memory_service_error)
    };
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_read(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_vmo_transfer(arguments).and_then(|(offset, bytes)| {
        services
            .read_vmo(receiver, offset, bytes)
            .map_err(status_from_memory_service_error)
    });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_write(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_vmo_transfer(arguments).and_then(|(offset, bytes)| {
        services
            .write_vmo(receiver, offset, bytes)
            .map_err(status_from_memory_service_error)
    });
    NativeAction::Return(status_only(result))
}

fn parse_vmo_transfer(
    arguments: &Arguments,
) -> Result<(u64, Option<UserSlice>), HyperNativeStatus> {
    if arguments[3] != 0 || arguments[4] != 0 || arguments[2] > HYPER_NATIVE_VMO_MAX_TRANSFER_BYTES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok((
        arguments[0],
        optional_user_slice(arguments[1], arguments[2])?,
    ))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_allocate(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[3..].iter().any(|value| *value != 0)
        || arguments[2] & !hyper::abi::native::HYPER_NATIVE_VMAR_ALLOCATE_EXACT != 0
    {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        services
            .allocate_vmar(receiver, arguments[0], arguments[1], arguments[2] != 0)
            .map_err(status_from_memory_service_error)
    };
    NativeAction::Return(match result {
        Ok((handle, address)) => success([handle_wire::to_abi(handle), address]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_map(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let vmar = receiver;
        let vmo = parse_handle(arguments[0])?;
        let permissions = crate::mm::user_space::abi_permissions(arguments[4])
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .map_vmo(
                vmar,
                vmo,
                arguments[1],
                arguments[2],
                arguments[3],
                permissions,
            )
            .map_err(status_from_memory_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_protect(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[3] != 0 || arguments[4] != 0 {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        (|| {
            let vmar = receiver;
            let permissions = crate::mm::user_space::abi_permissions(arguments[2])
                .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
            services
                .protect_vmar(vmar, arguments[0], arguments[1], permissions)
                .map_err(status_from_memory_service_error)
        })()
    };
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_unmap(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[2..].iter().any(|value| *value != 0) {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        services
            .unmap_vmar(receiver, arguments[0], arguments[1])
            .map_err(status_from_memory_service_error)
    };
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_destroy(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[0..].iter().any(|value| *value != 0) {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        services
            .destroy_vmar(receiver)
            .map_err(status_from_memory_service_error)
    };
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_create_snapshot(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        let value = receiver;
        services
            .create_vmo_snapshot(value)
            .map_err(status_from_memory_service_error)
    })();
    NativeAction::Return(match result {
        Ok((handle, size)) => success([handle_wire::to_abi(handle), size]),
        Err(error) => failure(error),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_map_private(
    services: &(impl MemoryServices + crate::abi::native::services::UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let vmar = receiver;
        let snapshot = parse_handle(arguments[0])?;
        let record = crate::abi::native::wire::copy_extensible_input_record::<48>(
            services,
            arguments[1],
            arguments[2],
            48,
        )?;
        use {
            crate::abi::native::wire::read_record_u32, crate::abi::native::wire::read_record_u64,
        };
        let request = crate::mm::user_space::PrivateMappingRequest {
            source_offset: read_record_u64(&record, 0),
            source_length: read_record_u64(&record, 8),
            address: read_record_u64(&record, 16),
            size: read_record_u64(&record, 24),
            data_offset: read_record_u64(&record, 32),
            permissions: read_record_u32(&record, 40),
            mode: read_record_u32(&record, 44),
        };
        services
            .map_private(vmar, snapshot, request)
            .map_err(status_from_memory_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_map_file(
    services: &(impl MemoryServices + crate::abi::native::services::UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let vmar = receiver;
        let file = parse_handle(arguments[0])?;
        let protection = parse_handle(arguments[1])?;
        let record = crate::abi::native::wire::copy_extensible_input_record::<48>(
            services,
            arguments[2],
            arguments[3],
            48,
        )?;
        use {
            crate::abi::native::wire::read_record_u32, crate::abi::native::wire::read_record_u64,
        };
        let request = crate::mm::user_space::PrivateMappingRequest {
            source_offset: read_record_u64(&record, 0),
            source_length: read_record_u64(&record, 8),
            address: read_record_u64(&record, 16),
            size: read_record_u64(&record, 24),
            data_offset: read_record_u64(&record, 32),
            permissions: read_record_u32(&record, 40),
            mode: read_record_u32(&record, 44),
        };
        services
            .map_file(vmar, file, protection, request)
            .map_err(status_from_memory_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_create_contiguous(
    services: &impl MemoryServices,
    arguments: &Arguments,
) -> NativeAction {
    let result = if arguments[1..].iter().any(|value| *value != 0) {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        services
            .create_contiguous_vmo(arguments[0])
            .map_err(status_from_memory_service_error)
    };
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_lock_memory(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    memory_residency(services, receiver, arguments, true)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmar_unlock_memory(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    memory_residency(services, receiver, arguments, false)
}

fn memory_residency(
    services: &impl MemoryServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
    lock: bool,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let current = match arguments[2] {
            0 => false,
            hyper::abi::native::HYPER_NATIVE_VMAR_MEMORY_CURRENT
                if arguments[0] == 0 && arguments[1] == 0 =>
            {
                true
            }
            _ => return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        };
        services
            .set_memory_residency(receiver, arguments[0], arguments[1], current, lock)
            .map_err(status_from_memory_service_error)
    })();
    NativeAction::Return(status_only(result))
}
