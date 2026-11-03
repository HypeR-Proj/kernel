// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability handle and object-kind records.

use super::{write_u32, write_u64};
use crate::capability::HandleInfo;
use hyper::abi::native::{
    HYPER_NATIVE_OBJECT_BYTE_CHANNEL, HYPER_NATIVE_OBJECT_CAPABILITY_CHANNEL,
    HYPER_NATIVE_OBJECT_CONSOLE, HYPER_NATIVE_OBJECT_CPU_INSPECTOR, HYPER_NATIVE_OBJECT_DIRECTORY,
    HYPER_NATIVE_OBJECT_EVENT, HYPER_NATIVE_OBJECT_EXECUTABLE_AUTHORITY, HYPER_NATIVE_OBJECT_FILE,
    HYPER_NATIVE_OBJECT_MEMORY_INSPECTOR, HYPER_NATIVE_OBJECT_OBJECT_INSPECTOR,
    HYPER_NATIVE_OBJECT_PENDING_VIRTUAL_MACHINE, HYPER_NATIVE_OBJECT_PROCESS,
    HYPER_NATIVE_OBJECT_PROCESS_BUILDER, HYPER_NATIVE_OBJECT_RESOURCE_DOMAIN,
    HYPER_NATIVE_OBJECT_TASK_FACTORY, HYPER_NATIVE_OBJECT_TASK_GROUP,
    HYPER_NATIVE_OBJECT_TASK_INSPECTOR, HYPER_NATIVE_OBJECT_THREAD,
    HYPER_NATIVE_OBJECT_VIRTUAL_CPU, HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE,
    HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_AUTHORITY,
    HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_LEASE, HYPER_NATIVE_OBJECT_VIRTUAL_SERIAL,
    HYPER_NATIVE_OBJECT_VMAR, HYPER_NATIVE_OBJECT_VMO, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HyperNativeHandleInfo, HyperNativeObjectBasicInfo, HyperNativeStatus,
};

pub(in crate::abi::native) const HANDLE_INFO_SIZE: usize =
    core::mem::size_of::<HyperNativeHandleInfo>();
pub(in crate::abi::native) const OBJECT_BASIC_INFO_SIZE: usize =
    core::mem::size_of::<HyperNativeObjectBasicInfo>();
pub(in crate::abi::native) fn parse_object_kind(
    raw: u32,
) -> Result<crate::object::ObjectKind, HyperNativeStatus> {
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

pub(in crate::abi::native) fn encode_handle_info(info: HandleInfo) -> [u8; HANDLE_INFO_SIZE] {
    encode_handle_info_fields(info.kind.get(), info.flags.bits(), info.rights.bits())
}

pub(in crate::abi::native) fn encode_handle_info_fields(
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

pub(in crate::abi::native) fn encode_object_basic_info(
    info: HandleInfo,
) -> [u8; OBJECT_BASIC_INFO_SIZE] {
    encode_object_basic_info_fields(info.koid.get(), info.kind.get())
}

pub(in crate::abi::native) fn encode_object_basic_info_fields(
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
