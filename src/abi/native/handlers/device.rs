// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native device syscall validation.

use crate::capability::InvocationReceiver;

use crate::abi::native::services::NativeAction;
use hyper::abi::native::{HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HyperNativeStatus};
use {crate::abi::native::Arguments, crate::abi::native::DeviceServices};
use {
    crate::abi::native::status::handle_result, crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_process_error,
    crate::abi::native::status::status_from_vm_service_error,
    crate::abi::native::status::status_only,
};
use {
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::optional_user_slice,
    crate::abi::native::wire::parse_handle, crate::abi::native::wire::parse_u32,
    crate::abi::native::wire::prepare_info_request, crate::abi::native::wire::require_zero,
};

fn status_from_match_error(
    error: crate::device::assigned::service::MatchError,
) -> HyperNativeStatus {
    use crate::device::assigned::service::MatchError;
    match error {
        MatchError::Service(error) => status_from_vm_service_error(error),
        MatchError::Missing => hyper::abi::native::HYPER_NATIVE_STATUS_NOT_FOUND,
        MatchError::Ambiguous => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_firmware_read(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let authority = receiver;
        if arguments[2] > 65536 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let mut query = [0u8; 24];
        let source =
            optional_user_slice(arguments[0], 24)?.ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .copy_from_user(source, &mut query)
            .map_err(status_from_process_error)?;
        let node = u32::from_le_bytes(
            query[..4]
                .try_into()
                .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
        );
        let field = u32::from_le_bytes(
            query[4..8]
                .try_into()
                .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
        );
        let name_address = u64::from_le_bytes(
            query[8..16]
                .try_into()
                .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
        );
        let name_length = u64::from_le_bytes(
            query[16..24]
                .try_into()
                .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
        );
        if field > 5 || name_length > 128 || (field == 4) != (name_length != 0) {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let mut name = [0u8; 128];
        let name_length =
            usize::try_from(name_length).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if let Some(source) = optional_user_slice(name_address, name_length as u64)? {
            services
                .copy_from_user(source, &mut name[..name_length])
                .map_err(status_from_process_error)?;
        }
        let name = core::str::from_utf8(&name[..name_length])
            .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if name.contains('\0') {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        // All firmware/device locks have been released before copying to user
        // memory, whose pages may fault or require a COW allocation.
        let bytes = services
            .device_firmware_read(authority, node, field, name)
            .map_err(status_from_match_error)?;
        if bytes.len() > 65536 {
            return Err(hyper::abi::native::HYPER_NATIVE_STATUS_INTERNAL);
        }
        let length = bytes.len() as u64;
        if arguments[2] == 0 {
            return Ok(length);
        }
        if arguments[2] < length {
            return Err(hyper::abi::native::HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
        }
        if let Some(destination) = optional_user_slice(arguments[1], length)? {
            services
                .copy_to_user(destination, &bytes)
                .map_err(status_from_process_error)?;
        }
        Ok(length)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_claim_bundle(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let authority = receiver;
        if arguments[1] == 0 || arguments[1] > 8 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let irq_node = parse_u32(arguments[2])?;
        let count =
            usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let mut bytes = [0u8; 8 * 16];
        let source = optional_user_slice(arguments[0], arguments[1] * 16)?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .copy_from_user(source, &mut bytes[..count * 16])
            .map_err(status_from_process_error)?;
        let mut entries = [(0, 0, 0); 8];
        for (entry, bytes) in entries[..count]
            .iter_mut()
            .zip(bytes[..count * 16].chunks_exact(16))
        {
            *entry = (
                u32::from_le_bytes(
                    bytes[..4]
                        .try_into()
                        .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
                ),
                u32::from_le_bytes(
                    bytes[4..8]
                        .try_into()
                        .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
                ),
                u64::from_le_bytes(
                    bytes[8..16]
                        .try_into()
                        .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?,
                ),
            );
        }
        services
            .device_claim_bundle(authority, &entries[..count], irq_node)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_mmio(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        if !matches!(arguments[1], 1 | 2 | 4)
            || arguments[2] > 1
            || (arguments[2] == 0 && arguments[3] != 0)
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .device_mmio(
                receiver,
                arguments[0],
                parse_u32(arguments[1])?,
                arguments[2] != 0,
                arguments[3],
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_irq_pending(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        services
            .device_irq_pending(receiver)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_irq_complete(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        if arguments[1] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .device_irq_complete(receiver, arguments[0], arguments[1] != 0)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_dma_fault_info(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let output = prepare_info_request(arguments, 40, 40)?;
        let bytes = services
            .device_dma_fault_info(receiver)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_profile_info(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let output = prepare_info_request(arguments, 32, 32)?;
        let bytes = services
            .device_profile_info(receiver)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_resource_info(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let output = prepare_info_request(&[arguments[1], arguments[2], 0, 0, 0, 0], 32, 32)?;
        let bytes = services
            .device_resource_info(receiver, parse_u32(arguments[0])?)
            .map_err(status_from_vm_service_error)?;
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_claim_matching(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let length =
            usize::try_from(arguments[3]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if length == 0 || length > 512 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let source = optional_user_slice(arguments[2], arguments[3])?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let mut bytes = [0u8; 512];
        services
            .copy_from_user(source, &mut bytes[..length])
            .map_err(status_from_process_error)?;
        let identity = core::str::from_utf8(&bytes[..length])
            .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .claim_device_matching(
                receiver,
                parse_u32(arguments[0])?,
                parse_u32(arguments[1])?,
                identity,
            )
            .map_err(status_from_match_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_device_claim(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        services
            .claim_device(receiver, parse_u32(arguments[0])?)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_physical_device_info(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let output = prepare_info_request(arguments, 16, 16)?;
        let info = services
            .physical_device_info(receiver)
            .map_err(status_from_vm_service_error)?;
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&info.device_id.to_le_bytes());
        bytes[4..8].copy_from_slice(&info.transport_version.to_le_bytes());
        bytes[8..16].copy_from_slice(&info.mmio_size.to_le_bytes());
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_vmo_get_dma_extent(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let output = prepare_info_request(&[arguments[3], arguments[4], 0, 0, 0, 0], 16, 16)?;
        let extent = services
            .vmo_dma_extent(
                receiver,
                parse_handle(arguments[0])?,
                arguments[1],
                arguments[2],
            )
            .map_err(status_from_vm_service_error)?;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&extent.physical_base.to_le_bytes());
        bytes[8..].copy_from_slice(&extent.length.to_le_bytes());
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_pending_virtual_machine_assign_device(
    services: &impl DeviceServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        services
            .assign_physical_device(
                receiver,
                parse_handle(arguments[0])?,
                arguments[1],
                parse_u32(arguments[2])?,
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}
