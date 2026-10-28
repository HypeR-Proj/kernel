// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native guest I/O syscall validation.

use crate::capability::handle_wire;

use crate::capability::InvocationReceiver;

use crate::abi::native::services::NativeAction;
use crate::vm::service::Error;
use hyper::abi::native::{
    HYPER_NATIVE_NATIVE_BLOCK_TRANSFER_FRAME_BYTES, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
};
use {crate::abi::native::Arguments, crate::abi::native::GuestIoServices};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::status_from_process_error,
    crate::abi::native::status::status_from_vm_service_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::optional_user_slice, crate::abi::native::wire::parse_handle,
    crate::abi::native::wire::parse_u32, crate::abi::native::wire::require_zero,
};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_mailbox_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        services
            .create_guest_mailbox(receiver, arguments[0], parse_u32(arguments[1])?)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_mailbox_send(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let length =
            usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if length == 0 || length > 256 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let source = optional_user_slice(arguments[0], arguments[1])?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let mailbox = receiver;
        let mut bytes = [0u8; 256];
        services
            .copy_from_user(source, &mut bytes[..length])
            .map_err(status_from_process_error)?;
        services
            .send_guest_mailbox(mailbox, &bytes[..length])
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_mailbox_receive(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        if arguments[1] == 0 || arguments[1] > 256 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let _validated = optional_user_slice(arguments[0], arguments[1])?;
        let mailbox = receiver;
        services
            .receive_guest_mailbox(mailbox, &mut |bytes| {
                if bytes.len() as u64 > arguments[1] {
                    return Err(Error::InvalidArgument);
                }
                let destination =
                    UserSlice::new(UserAddress::new(arguments[0]), bytes.len() as u64)
                        .map_err(|_| Error::Fault)?;
                services
                    .copy_to_user(destination, bytes)
                    .map_err(|_| Error::Fault)
            })
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok(length) => success([length as u64, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_notification_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        services
            .create_guest_notification(
                receiver,
                parse_handle(arguments[0])?,
                arguments[1],
                arguments[2],
                parse_u32(arguments[3])?,
                parse_u32(arguments[4])?,
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_notification_control(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        services
            .control_guest_notification(receiver, parse_u32(arguments[0])?)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok(epoch) => success([u64::from(epoch), 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_native_block_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        services
            .create_native_block(
                receiver,
                parse_handle(arguments[0])?,
                arguments[1],
                arguments[2],
                parse_u32(arguments[3])?,
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_native_block_activate(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        if arguments[0] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .activate_native_block(receiver, arguments[0] != 0)
            .map_err(|error| {
                use crate::block::service::ActivationError;
                use crate::fs::primitives::block::Error;
                use hyper::abi::native::{
                    HYPER_NATIVE_STATUS_IO_ERROR, HYPER_NATIVE_STATUS_NOT_SUPPORTED,
                    HYPER_NATIVE_STATUS_READ_ONLY, HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
                };
                match error {
                    ActivationError::Capability(error) => status_from_process_error(error),
                    ActivationError::Device(error) => match error {
                        Error::InvalidRange => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
                        Error::ReadOnly => HYPER_NATIVE_STATUS_READ_ONLY,
                        Error::Disconnected | Error::Io | Error::Corrupt => {
                            HYPER_NATIVE_STATUS_IO_ERROR
                        }
                        Error::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
                        Error::Exhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
                    },
                }
            })
    })();
    NativeAction::Return(match result {
        Ok((sectors, readonly)) => success([sectors, u64::from(readonly)]),
        Err(error) => failure(error),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_mount(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[4] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        if arguments[3] == 0 || arguments[3] > 4096 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let path = UserSlice::new(UserAddress::new(arguments[2]), arguments[3])
            .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .mount_filesystem(
                parse_handle(arguments[0])?,
                parse_handle(arguments[1])?,
                if arguments[5] == 0 {
                    None
                } else {
                    Some(parse_handle(arguments[5])?)
                },
                receiver,
                path,
                arguments[4] != 0,
            )
            .map_err(crate::abi::native::status::status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_unmount(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        if arguments[1] == 0 || arguments[1] > 4096 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let path = UserSlice::new(UserAddress::new(arguments[0]), arguments[1])
            .map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .unmount_filesystem(receiver, path)
            .map_err(crate::abi::native::status::status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_native_block_transfer(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let operation = parse_u32(arguments[0])?;
        if operation > 3
            || arguments[3] > HYPER_NATIVE_NATIVE_BLOCK_TRANSFER_FRAME_BYTES
            || (operation == 2 && (arguments[1] != 0 || arguments[2] != 0 || arguments[3] != 0))
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let buffer = optional_user_slice(arguments[2], arguments[3])?;
        services
            .transfer_native_block(receiver, operation, arguments[1], buffer)
            .map_err(|error| {
                use crate::block::service::TransferError;
                use {crate::fs::primitives::block::Error, hyper::abi::native::*};
                match error {
                    TransferError::Capability(error) => status_from_process_error(error),
                    TransferError::Status(status) => status,
                    TransferError::Device(error) => match error {
                        Error::InvalidRange => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
                        Error::ReadOnly => HYPER_NATIVE_STATUS_READ_ONLY,
                        Error::Unsupported => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
                        Error::Exhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
                        _ => HYPER_NATIVE_STATUS_IO_ERROR,
                    },
                }
            })
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_mapping_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        services
            .create_guest_mapping(receiver, parse_handle(arguments[0])?, arguments[1])
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok((handle, token)) => success([handle_wire::to_abi(handle), token]),
        Err(error) => failure(error),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_mapping_release(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        services
            .release_guest_mapping(receiver)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_notification_configure(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        services
            .configure_guest_notification(
                receiver,
                arguments[0],
                arguments[1],
                parse_u32(arguments[2])?,
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok(()) => success([0, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_notification_access(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        services
            .access_guest_notification(
                receiver,
                arguments[0],
                parse_u32(arguments[1])?,
                parse_u32(arguments[2])?,
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok(value) => success([u64::from(value), 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_memory_transfer(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        if arguments[0] > 1 || arguments[3] == 0 || arguments[3] > 4096 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let memory = receiver;
        let buffer = optional_user_slice(arguments[2], arguments[3])?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .transfer_guest_memory(memory, arguments[0] != 0, arguments[1], buffer)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_memory_scalar(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        if arguments[0] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .scalar_guest_memory(
                receiver,
                arguments[0] != 0,
                arguments[1],
                parse_u32(arguments[2])?,
                arguments[3],
            )
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(match result {
        Ok(value) => success([value, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_interrupt_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        services
            .create_guest_interrupt(receiver, parse_u32(arguments[0])?)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_guest_interrupt_set_level(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        if arguments[0] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .set_guest_interrupt_level(receiver, arguments[0] != 0)
            .map_err(status_from_vm_service_error)
    })();
    NativeAction::Return(status_only(result))
}
