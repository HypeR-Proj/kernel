// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One schema-owned private/file mapping record shared by both operations.

use super::{copy_extensible_input_record, read_record_u32, read_record_u64};
use crate::abi::native::services::UserMemoryServices;
use crate::mm::user_space::PrivateMappingRequest;
use hyper::abi::native::{
    HYPER_NATIVE_PRIVATE_MAPPING_MIN_SIZE, HyperNativePrivateMapping, HyperNativeStatus,
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
