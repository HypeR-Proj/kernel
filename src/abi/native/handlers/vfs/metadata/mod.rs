// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native filesystem metadata records and syscall leaves.

use super::parse_path;
use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use crate::time::queue::Timestamp;
use hyper::abi::native as abi;
use {
    crate::abi::native::services::NativeAction, crate::abi::native::services::UserMemoryServices,
    crate::abi::native::services::VfsServices,
};
use {
    crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_vfs_service_error,
    crate::abi::native::status::status_only,
};
use {
    crate::abi::native::wire::InfoRequest, crate::abi::native::wire::copy_extensible_input_record,
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::prepare_info_request,
    crate::abi::native::wire::require_zero,
};
use {crate::fs::vfs::Metadata, crate::fs::vfs::MetadataUpdate};

type Status = abi::HyperNativeStatus;
type MetadataRecord = abi::HyperNativeFileMetadata;
type UpdateRecord = abi::HyperNativeFileMetadataUpdate;
const METADATA_SIZE: usize = core::mem::size_of::<MetadataRecord>();
const UPDATE_SIZE: usize = core::mem::size_of::<UpdateRecord>();
const INVALID: Status = abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT;

fn parse_follow_symlinks(options: u64) -> Result<bool, Status> {
    match options {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(INVALID),
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod self_test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(in crate::abi::native) use self_test::run as run_wire_self_test;

fn metadata_request(address: u64, capacity: u64) -> Result<InfoRequest, Status> {
    prepare_info_request(
        &[address, capacity, 0, 0, 0, 0],
        abi::HYPER_NATIVE_FILE_METADATA_MIN_SIZE,
        METADATA_SIZE,
    )
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn encode_metadata(value: Metadata) -> [u8; METADATA_SIZE] {
    let mut bytes = [0; METADATA_SIZE];
    put_u64(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, filesystem_id),
        value.location.filesystem_id,
    );
    put_u64(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, mount_id),
        value.location.mount_id,
    );
    put_u64(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, node_id),
        value.location.node_id,
    );
    put_u64(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, size),
        value.attributes.size(),
    );
    put_u32(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, mode),
        value.attributes.mode(),
    );
    let kind = match value.attributes.kind() {
        crate::fs::primitives::NodeKind::File => abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_FILE as u32,
        crate::fs::primitives::NodeKind::Directory => {
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_DIRECTORY as u32
        }
        crate::fs::primitives::NodeKind::Symlink => {
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_SYMLINK as u32
        }
        crate::fs::primitives::NodeKind::Other => {
            abi::HYPER_NATIVE_DIRECTORY_ENTRY_KIND_OTHER as u32
        }
    };
    put_u32(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, kind),
        kind,
    );
    let mut valid = 0;
    let offsets = [
        (
            core::mem::offset_of!(MetadataRecord, accessed_seconds),
            core::mem::offset_of!(MetadataRecord, accessed_nanoseconds),
        ),
        (
            core::mem::offset_of!(MetadataRecord, modified_seconds),
            core::mem::offset_of!(MetadataRecord, modified_nanoseconds),
        ),
        (
            core::mem::offset_of!(MetadataRecord, created_seconds),
            core::mem::offset_of!(MetadataRecord, created_nanoseconds),
        ),
        (
            core::mem::offset_of!(MetadataRecord, changed_seconds),
            core::mem::offset_of!(MetadataRecord, changed_nanoseconds),
        ),
    ];
    for (index, timestamp) in [value.accessed, value.modified, value.created, value.changed]
        .into_iter()
        .enumerate()
    {
        if let Some(timestamp) = timestamp {
            valid |= 1 << index;
            put_u64(&mut bytes, offsets[index].0, timestamp.seconds() as u64);
            put_u32(&mut bytes, offsets[index].1, timestamp.nanoseconds());
        }
    }
    put_u32(
        &mut bytes,
        core::mem::offset_of!(MetadataRecord, valid_times),
        valid,
    );
    bytes
}

fn decode_metadata_update(
    services: &impl UserMemoryServices,
    address: u64,
    size: u64,
) -> Result<MetadataUpdate, Status> {
    let bytes = copy_extensible_input_record::<UPDATE_SIZE>(
        services,
        address,
        size,
        abi::HYPER_NATIVE_FILE_METADATA_UPDATE_MIN_SIZE,
    )?;
    let word = |offset: usize| -> Result<u32, Status> {
        Ok(u32::from_le_bytes(
            bytes[offset..offset + 4].try_into().map_err(|_| INVALID)?,
        ))
    };
    let mask = word(core::mem::offset_of!(UpdateRecord, mask))?;
    let mode = word(core::mem::offset_of!(UpdateRecord, mode))?;
    if mask & !7 != 0
        || mode & !0o777 != 0
        || word(core::mem::offset_of!(UpdateRecord, accessed_reserved))? != 0
        || word(core::mem::offset_of!(UpdateRecord, modified_reserved))? != 0
        || (mask & 1 == 0 && mode != 0)
    {
        return Err(INVALID);
    }
    let time =
        |offset: usize, nanos_offset: usize, selected: bool| -> Result<Option<Timestamp>, Status> {
            let seconds =
                i64::from_le_bytes(bytes[offset..offset + 8].try_into().map_err(|_| INVALID)?);
            let nanos = word(nanos_offset)?;
            if !selected {
                return if seconds == 0 && nanos == 0 {
                    Ok(None)
                } else {
                    Err(INVALID)
                };
            }
            Timestamp::new(seconds, nanos).map(Some).ok_or(INVALID)
        };
    Ok(MetadataUpdate {
        mode: (mask & 1 != 0).then_some(mode),
        accessed: time(
            core::mem::offset_of!(UpdateRecord, accessed_seconds),
            core::mem::offset_of!(UpdateRecord, accessed_nanoseconds),
            mask & 2 != 0,
        )?,
        modified: time(
            core::mem::offset_of!(UpdateRecord, modified_seconds),
            core::mem::offset_of!(UpdateRecord, modified_nanoseconds),
            mask & 4 != 0,
        )?,
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_get_metadata(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let request = metadata_request(arguments[3], arguments[4])?;
        let value = services
            .directory_get_metadata(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_follow_symlinks(arguments[2])?,
            )
            .map_err(status_from_vfs_service_error)?;
        copy_info_record(services, request, &encode_metadata(value))
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_set_metadata(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let update = decode_metadata_update(services, arguments[3], arguments[4])?;
        services
            .directory_set_metadata(
                receiver,
                parse_path(arguments[0], arguments[1])?,
                parse_follow_symlinks(arguments[2])?,
                update,
            )
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_set_metadata(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let update = decode_metadata_update(services, arguments[0], arguments[1])?;
        services
            .file_set_metadata(receiver, update)
            .map_err(status_from_vfs_service_error)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_file_get_metadata(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let request = metadata_request(arguments[0], arguments[1])?;
        let value = services
            .file_get_metadata(receiver)
            .map_err(status_from_vfs_service_error)?;
        copy_info_record(services, request, &encode_metadata(value))
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_directory_get_self_metadata(
    services: &impl VfsServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let request = metadata_request(arguments[0], arguments[1])?;
        let value = services
            .directory_get_self_metadata(receiver)
            .map_err(status_from_vfs_service_error)?;
        copy_info_record(services, request, &encode_metadata(value))
    })();
    NativeAction::Return(info_result(result))
}
