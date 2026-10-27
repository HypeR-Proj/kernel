// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native directory traversal and file I/O syscalls.

use crate::capability::handle_wire;

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use crate::capability::Rights;
use hyper::abi::native as abi;
use hyper::abi::native::{
    HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY, HYPER_NATIVE_DIRECTORY_INFO_MIN_SIZE,
    HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES, HYPER_NATIVE_FILE_INFO_MIN_SIZE,
    HYPER_NATIVE_FILE_MAX_READ_BYTES, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HyperNativeDirectoryEntry, HyperNativeDirectoryInfo, HyperNativeFileInfo, HyperNativeStatus,
};
use {crate::abi::native::services::NativeAction, crate::abi::native::services::VfsServices};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::info_result, crate::abi::native::status::scan_result,
    crate::abi::native::status::status_from_address_error,
    crate::abi::native::status::status_from_process_error,
    crate::abi::native::status::status_from_vfs_service_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::copy_directory_page, crate::abi::native::wire::copy_info_record,
    crate::abi::native::wire::encode_directory_info, crate::abi::native::wire::encode_file_info,
    crate::abi::native::wire::optional_user_slice, crate::abi::native::wire::parse_handle,
    crate::abi::native::wire::prepare_info_request, crate::abi::native::wire::require_zero,
};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};
mod metadata;
mod namespace;
pub(in crate::abi::native) use namespace::{
    sys_filesystem_begin_root_switch, sys_filesystem_finish_root_switch,
    sys_filesystem_mount_ramfs, sys_filesystem_switch_root,
};

type Status = abi::HyperNativeStatus;
const INVALID: Status = abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT;

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_open_file(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_directory_open(arguments).and_then(|(path, rights)| {
        services
            .open_file(receiver, path, rights)
            .map_err(status_from_vfs_service_error)
    });
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_open_directory(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_directory_open(arguments).and_then(|(path, rights)| {
        services
            .open_directory(receiver, path, rights)
            .map_err(status_from_vfs_service_error)
    });
    NativeAction::Return(handle_result(result))
}

fn parse_directory_open(arguments: &Arguments) -> Result<(UserSlice, Rights), HyperNativeStatus> {
    if arguments[3] != 0
        || arguments[4] != 0
        || arguments[1] == 0
        || arguments[1] > HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let path = UserSlice::new(UserAddress::new(arguments[0]), arguments[1])
        .map_err(status_from_address_error)?;
    let rights = Rights::from_bits(arguments[2]).ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    Ok((path, rights))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_begin_read(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        let directory = receiver;
        services
            .begin_directory_read(directory)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_read(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_directory_read(arguments).and_then(|(cookie, destination)| {
        let mut page = crate::fs::vfs::DirectoryPage::empty();
        services
            .read_directory(receiver, cookie, &mut page)
            .map_err(status_from_vfs_service_error)?;
        copy_directory_page(services, destination, &page)
    });
    NativeAction::Return(scan_result(result))
}

fn parse_directory_read(arguments: &Arguments) -> Result<(u64, UserSlice), HyperNativeStatus> {
    if arguments[2] != HYPER_NATIVE_DIRECTORY_ENTRY_PAGE_CAPACITY
        || arguments[3] != 0
        || arguments[4] != 0
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let bytes = arguments[2]
        .checked_mul(core::mem::size_of::<HyperNativeDirectoryEntry>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let destination =
        UserSlice::new(UserAddress::new(arguments[1]), bytes).map_err(status_from_address_error)?;
    Ok((arguments[0], destination))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_read_at(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let file = receiver;
        if arguments[0] != 0 || arguments[3] > HYPER_NATIVE_FILE_MAX_READ_BYTES {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let output = optional_user_slice(arguments[2], arguments[3])?;
        services
            .read_file_at(file, arguments[1], output)
            .map_err(status_from_vfs_service_error)
    })();
    let result = match result {
        Ok((actual, file_size)) => success([actual, file_size]),
        Err(status) => failure(status),
    };
    NativeAction::Return(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_get_info(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_FILE_INFO_MIN_SIZE,
        core::mem::size_of::<HyperNativeFileInfo>(),
    )
    .and_then(|request| {
        let file = receiver;
        let info = services
            .file_info(file)
            .map_err(status_from_vfs_service_error)?;
        copy_info_record(services, request, &encode_file_info(info))
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_get_info(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_DIRECTORY_INFO_MIN_SIZE,
        core::mem::size_of::<HyperNativeDirectoryInfo>(),
    )
    .and_then(|request| {
        let directory = receiver;
        let info = services
            .directory_info(directory)
            .map_err(status_from_vfs_service_error)?;
        copy_info_record(services, request, &encode_directory_info(info))
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_write_at(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[0] > 1
            || (arguments[0] == 1 && arguments[1] != 0)
            || arguments[3] > HYPER_NATIVE_FILE_MAX_READ_BYTES
            || arguments[4] != 0
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let file = receiver;
        let input = optional_user_slice(arguments[2], arguments[3])?;
        services
            .write_file_at(file, (arguments[0] == 0).then_some(arguments[1]), input)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(match result {
        Ok((actual, end)) => success([actual, end]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_resize(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[1..].iter().any(|value| *value != 0) {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .resize_file(receiver, arguments[0])
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(match result {
        Ok(()) => success([0, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_create_file(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[4] != 0 || arguments[3] & !0o777 != 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let path = parse_path(arguments[0], arguments[1])?;
        let rights = Rights::from_bits(arguments[2]).ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .create_file(receiver, path, rights, arguments[3] as u32)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_create_directory(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[3] != 0 || arguments[4] != 0 || arguments[2] & !0o777 != 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let path = parse_path(arguments[0], arguments[1])?;
        services
            .create_directory(receiver, path, arguments[2] as u32)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(match result {
        Ok(()) => success([0, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_remove(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[3] != 0 || arguments[4] != 0 || arguments[2] > 1 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let path = parse_path(arguments[0], arguments[1])?;
        services
            .remove_entry(receiver, path, arguments[2] == 1)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(match result {
        Ok(()) => success([0, 0]),
        Err(status) => failure(status),
    })
}

fn parse_path(address: u64, length: u64) -> Result<UserSlice, Status> {
    if length == 0 || length > abi::HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES {
        return Err(INVALID);
    }
    UserSlice::new(UserAddress::new(address), length).map_err(status_from_address_error)
}

fn parse_rights(value: u64) -> Result<Rights, Status> {
    Rights::from_bits(value).ok_or(INVALID)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_scope_create(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        services
            .directory_scope_create(
                receiver,
                parse_handle(arguments[0])?,
                parse_rights(arguments[1])?,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_symlink(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        services
            .directory_symlink(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_path(arguments[2], arguments[3])?,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_remove_if(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let is_directory = match arguments[2] {
            0 => false,
            1 => true,
            _ => return Err(INVALID),
        };
        services
            .directory_remove_if(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                is_directory,
                arguments[3],
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_open_directory_nofollow(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        services
            .directory_open_directory_nofollow(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_rights(arguments[2])?,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_sync(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[1..])?;
        if arguments[0] > 1 {
            return Err(INVALID);
        }
        services
            .file_sync(receiver, arguments[0])
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_lock(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let mode = match arguments[0] {
            0 => crate::fs::vfs::locks::LockMode::Shared,
            1 => crate::fs::vfs::locks::LockMode::Exclusive,
            _ => return Err(INVALID),
        };
        services
            .file_lock(receiver, mode, arguments[1])
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_unlock(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        services
            .file_unlock(receiver)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_open_file_with_options(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let mode = u32::try_from(arguments[4]).map_err(|_| INVALID)?;
        services
            .directory_open_file_with_options(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_rights(arguments[2])?,
                arguments[3],
                mode,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_rename(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        services
            .directory_rename(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_handle(arguments[2])?,
                parse_path(arguments[3], arguments[4])?,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_link(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        services
            .directory_link(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_handle(arguments[2])?,
                parse_path(arguments[3], arguments[4])?,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_read_link(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        if arguments[3] > abi::HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES {
            return Err(INVALID);
        }
        let value = services
            .directory_read_link(receiver, parse_path(arguments[0], arguments[1])?)
            .map_err(status_from_vfs_service_error)?;
        let bytes = &value[..];
        let length = bytes.len() as u64;
        if arguments[3] < length {
            return Err(abi::HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
        }
        if length != 0 {
            let destination = UserSlice::new(UserAddress::new(arguments[2]), length)
                .map_err(status_from_address_error)?;
            services
                .copy_to_user(destination, bytes)
                .map_err(status_from_process_error)?;
        }
        Ok(length)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_canonicalize(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        if arguments[3] > abi::HYPER_NATIVE_DIRECTORY_MAX_PATH_BYTES {
            return Err(INVALID);
        }
        let value = services
            .directory_canonicalize(receiver, parse_path(arguments[0], arguments[1])?)
            .map_err(status_from_vfs_service_error)?;
        let bytes = value.as_bytes();
        let length = bytes.len() as u64;
        if arguments[3] < length {
            return Err(abi::HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
        }
        if length != 0 {
            let destination = UserSlice::new(UserAddress::new(arguments[2]), length)
                .map_err(status_from_address_error)?;
            services
                .copy_to_user(destination, bytes)
                .map_err(status_from_process_error)?;
        }
        Ok(length)
    })();
    NativeAction::Return(info_result(result))
}

pub(in crate::abi::native) use metadata::{
    sys_directory_get_metadata, sys_directory_get_self_metadata, sys_directory_set_metadata,
    sys_file_get_metadata, sys_file_set_metadata,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(in crate::abi::native) use metadata::run_wire_self_test;

#[inline(never)]
pub(in crate::abi::native) fn sys_file_protect(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[0..])?;
        let file = receiver;
        services
            .protect_file(file)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(match result {
        Ok((handle, size)) => success([handle_wire::to_abi(handle), size]),
        Err(error) => failure(error),
    })
}
