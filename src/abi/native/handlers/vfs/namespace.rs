// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit namespace administration; no boot layout is implied by these calls.

use crate::abi::native::Arguments;
use crate::abi::native::wire::require_zero;
use crate::capability::InvocationReceiver;
use hyper::abi::native as abi;
use {crate::abi::native::services::NativeAction, crate::abi::native::services::VfsServices};
use {
    crate::abi::native::status::status_from_address_error,
    crate::abi::native::status::status_from_vfs_service_error,
    crate::abi::native::status::status_only,
};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};

fn path(address: u64, length: u64) -> Result<UserSlice, abi::HyperNativeStatus> {
    if length == 0 || length > abi::HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES {
        return Err(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    UserSlice::new(UserAddress::new(address), length).map_err(status_from_address_error)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_mount_ramfs(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        services
            .mount_ramfs(receiver, path(arguments[0], arguments[1])?)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_switch_root(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        let request = UserSlice::new(
            UserAddress::new(arguments[0]),
            abi::HYPER_NATIVE_FILESYSTEM_ROOT_SWITCH_MIN_SIZE as u64,
        )
        .map_err(status_from_address_error)?;
        services
            .switch_root(receiver, request)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_begin_root_switch(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        let request = UserSlice::new(
            UserAddress::new(arguments[0]),
            abi::HYPER_NATIVE_FILESYSTEM_ROOT_SWITCH_MIN_SIZE as u64,
        )
        .map_err(status_from_address_error)?;
        services
            .begin_root_switch(receiver, request)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(crate::abi::native::status::handle_result(result))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_filesystem_finish_root_switch(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        let commit = match arguments[0] {
            0 => false,
            1 => true,
            _ => return Err(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        };
        services
            .finish_root_switch(receiver, commit)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}
