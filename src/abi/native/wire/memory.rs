// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Schema-owned private and shared file mapping request decoding.

use super::{copy_extensible_input_record, read_record_u32, read_record_u64};
use crate::abi::native::services::UserMemoryServices;
use crate::mm::user_space::{
    PrivateMappingRequest, SharedFileMappingRequest, SharedFileSyncRequest,
};
use hyper::abi::native::{
    HYPER_NATIVE_PRIVATE_MAPPING_MIN_SIZE, HYPER_NATIVE_SHARED_FILE_MAPPING_MIN_SIZE,
    HYPER_NATIVE_SHARED_FILE_SYNC_MIN_SIZE, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HyperNativePrivateMapping, HyperNativeSharedFileMapping, HyperNativeSharedFileSync,
    HyperNativeStatus,
};

pub(in crate::abi::native) fn decode_private_mapping(
    services: &impl UserMemoryServices,
    address: u64,
    length: u64,
) -> Result<PrivateMappingRequest, HyperNativeStatus> {
    type Record = HyperNativePrivateMapping;
    const SIZE: usize = core::mem::size_of::<Record>();
    let record = copy_extensible_input_record::<SIZE>(
        services,
        address,
        length,
        HYPER_NATIVE_PRIVATE_MAPPING_MIN_SIZE,
    )?;
    Ok(PrivateMappingRequest {
        source_offset: read_record_u64(&record, core::mem::offset_of!(Record, source_offset)),
        source_length: read_record_u64(&record, core::mem::offset_of!(Record, source_length)),
        address: read_record_u64(&record, core::mem::offset_of!(Record, address)),
        size: read_record_u64(&record, core::mem::offset_of!(Record, size)),
        data_offset: read_record_u64(&record, core::mem::offset_of!(Record, data_offset)),
        permissions: read_record_u32(&record, core::mem::offset_of!(Record, permissions)),
        mode: read_record_u32(&record, core::mem::offset_of!(Record, mode)),
    })
}

pub(in crate::abi::native) fn decode_shared_file_mapping(
    services: &impl UserMemoryServices,
    address: u64,
    length: u64,
) -> Result<SharedFileMappingRequest, HyperNativeStatus> {
    type Record = HyperNativeSharedFileMapping;
    const SIZE: usize = core::mem::size_of::<Record>();
    let record = copy_extensible_input_record::<SIZE>(
        services,
        address,
        length,
        HYPER_NATIVE_SHARED_FILE_MAPPING_MIN_SIZE,
    )?;
    let options = read_record_u32(&record, core::mem::offset_of!(Record, options));
    if options != 0
        || read_record_u64(&record, core::mem::offset_of!(Record, reserved0)) != 0
        || read_record_u64(&record, core::mem::offset_of!(Record, reserved1)) != 0
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(SharedFileMappingRequest {
        file_offset: read_record_u64(&record, core::mem::offset_of!(Record, file_offset)),
        address: read_record_u64(&record, core::mem::offset_of!(Record, address)),
        length: read_record_u64(&record, core::mem::offset_of!(Record, length)),
        permissions: read_record_u32(&record, core::mem::offset_of!(Record, permissions)),
        options,
    })
}

pub(in crate::abi::native) fn decode_shared_file_sync(
    services: &impl UserMemoryServices,
    address: u64,
    length: u64,
) -> Result<SharedFileSyncRequest, HyperNativeStatus> {
    type Record = HyperNativeSharedFileSync;
    const SIZE: usize = core::mem::size_of::<Record>();
    let record = copy_extensible_input_record::<SIZE>(
        services,
        address,
        length,
        HYPER_NATIVE_SHARED_FILE_SYNC_MIN_SIZE,
    )?;
    let options = read_record_u32(&record, core::mem::offset_of!(Record, options));
    let scope = read_record_u32(&record, core::mem::offset_of!(Record, scope));
    if options != 0
        || !matches!(
            u64::from(scope),
            hyper::abi::native::HYPER_NATIVE_FILE_SYNC_DATA
                | hyper::abi::native::HYPER_NATIVE_FILE_SYNC_ALL
        )
        || read_record_u64(&record, core::mem::offset_of!(Record, reserved0)) != 0
        || read_record_u64(&record, core::mem::offset_of!(Record, reserved1)) != 0
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(SharedFileSyncRequest {
        address: read_record_u64(&record, core::mem::offset_of!(Record, address)),
        length: read_record_u64(&record, core::mem::offset_of!(Record, length)),
        scope,
        options,
    })
}
