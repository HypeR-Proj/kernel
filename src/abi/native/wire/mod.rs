// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native wire-record decoding, validation, and byte encoding.

use super::Arguments;
use super::services::UserMemoryServices;
use super::status::{
    status_from_address_error, status_from_handle_error, status_from_process_error,
};
use crate::capability::{HandleValue, handle_wire};
use crate::diagnostics::inspect::Page;
use crate::mm::user_space::{UserAddress, UserSlice};
use hyper::abi::native::{
    HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES, HYPER_NATIVE_STATUS_INTERNAL,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HyperNativeStatus,
};

mod memory;
pub(super) use memory::{
    decode_private_mapping, decode_shared_file_mapping, decode_shared_file_sync,
};
pub(super) mod memory_statistics;
mod object_details;
pub(super) use object_details::encode_object_details;

mod resource;
pub(super) use resource::decode_resource_limits;
mod vm;
pub(super) use vm::{
    decode_virtual_cpu_bootstrap, decode_virtual_machine_configuration, encode_virtual_cpu_info,
    encode_virtual_cpu_mmio_request, encode_virtual_machine_info,
    encode_virtual_machine_platform_info, encode_virtual_machine_power_request,
    parse_virtual_serial_io,
};
mod ipc;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use ipc::{
    capability_disposition_bytes, capability_receive_slot_bytes, capability_record_bytes,
};
pub(super) use ipc::{
    parse_byte_channel_io, parse_capability_channel_receive, parse_capability_channel_send,
    parse_wait_many,
};
mod process;
pub(super) use process::{
    PROCESS_INFO_SIZE, decode_thread_create, encode_process_info, parse_affinity_request,
    parse_builder_create, parse_builder_handle, parse_builder_text, terminal_reason,
};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use process::{ProcessBuilderHandleRequest, encode_process_info_fields};
mod object;
pub(super) use object::{
    HANDLE_INFO_SIZE, OBJECT_BASIC_INFO_SIZE, encode_handle_info, encode_object_basic_info,
};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use object::{
    encode_handle_info_fields, encode_object_basic_info_fields, parse_object_kind,
};
mod console;
pub(super) use console::parse_console_io;
mod inspect;
pub(super) use inspect::{
    encode_cpu_observation, encode_handle_inspection, encode_memory_observation,
    encode_object_inspection, encode_task_process, encode_task_thread, parse_handle_inspector_scan,
    parse_inspector_scan,
};
mod filesystem;
pub(super) use filesystem::{copy_directory_page, encode_directory_info, encode_file_info};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) use filesystem::{encode_directory_info_fields, encode_file_info_fields};

pub(super) fn parse_u32(value: u64) -> Result<u32, HyperNativeStatus> {
    u32::try_from(value).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}

pub(super) fn parse_handle(raw: u64) -> Result<HandleValue, HyperNativeStatus> {
    handle_wire::from_abi(raw).map_err(status_from_handle_error)
}

pub(super) fn read_record_u32(record: &[u8], offset: usize) -> u32 {
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&record[offset..offset + 4]);
    u32::from_ne_bytes(bytes)
}

pub(super) fn read_record_u64(record: &[u8], offset: usize) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&record[offset..offset + 8]);
    u64::from_ne_bytes(bytes)
}

pub(super) fn optional_user_slice(
    raw_address: u64,
    length: u64,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if length == 0 {
        return Ok(None);
    }
    UserSlice::new(UserAddress::new(raw_address), length)
        .map(Some)
        .map_err(status_from_address_error)
}

pub(super) fn require_zero(arguments: &[u64]) -> Result<(), HyperNativeStatus> {
    if arguments.iter().all(|argument| *argument == 0) {
        Ok(())
    } else {
        Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    }
}

#[derive(Clone, Copy)]
pub(super) struct InfoRequest {
    pub(super) destination: UserSlice,
    pub(super) supported_size: usize,
}

pub(super) fn prepare_info_request(
    arguments: &Arguments,
    minimum_size: usize,
    supported_size: usize,
) -> Result<InfoRequest, HyperNativeStatus> {
    require_zero(&arguments[2..])?;
    let capacity =
        usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let maximum = HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES as usize;
    if minimum_size == 0
        || minimum_size > supported_size
        || capacity < minimum_size
        || capacity > maximum
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let write_size = capacity.min(supported_size);
    let write_size = u64::try_from(write_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    let destination = UserSlice::new(UserAddress::new(arguments[0]), write_size)
        .map_err(status_from_address_error)?;
    Ok(InfoRequest {
        destination,
        supported_size,
    })
}

pub(super) fn copy_info_record(
    services: &impl UserMemoryServices,
    request: InfoRequest,
    record: &[u8],
) -> Result<u64, HyperNativeStatus> {
    let write_size =
        usize::try_from(request.destination.length()).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
    if record.len() != request.supported_size || write_size > record.len() {
        return Err(HYPER_NATIVE_STATUS_INTERNAL);
    }
    services
        .copy_to_user(request.destination, &record[..write_size])
        .map_err(status_from_process_error)?;
    u64::try_from(request.supported_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)
}

pub(super) fn copy_extensible_input_record<const SIZE: usize>(
    services: &impl UserMemoryServices,
    address: u64,
    requested_size: u64,
    minimum_size: usize,
) -> Result<[u8; SIZE], HyperNativeStatus> {
    const EXTENSION_CHUNK_BYTES: usize = 64;

    let requested =
        usize::try_from(requested_size).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let maximum = HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES as usize;
    if minimum_size == 0 || minimum_size > SIZE || requested < minimum_size || requested > maximum {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let base = UserAddress::new(address);
    let prefix_size = requested.min(SIZE);
    let prefix = UserSlice::new(
        base,
        u64::try_from(prefix_size).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
    )
    .map_err(status_from_address_error)?;
    let mut record = [0_u8; SIZE];
    services
        .copy_from_user(prefix, &mut record[..prefix_size])
        .map_err(status_from_process_error)?;

    let mut extension = [0_u8; EXTENSION_CHUNK_BYTES];
    let mut offset = SIZE;
    while offset < requested {
        let length = (requested - offset).min(extension.len());
        let raw_offset = u64::try_from(offset).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        let extension_base = base
            .checked_add(raw_offset)
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let source = UserSlice::new(
            extension_base,
            u64::try_from(length).map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?,
        )
        .map_err(status_from_address_error)?;
        services
            .copy_from_user(source, &mut extension[..length])
            .map_err(status_from_process_error)?;
        if extension[..length].iter().any(|byte| *byte != 0) {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        offset += length;
    }
    Ok(record)
}

pub(super) fn validate_record_stride(
    stride: u64,
    minimum: usize,
) -> Result<usize, HyperNativeStatus> {
    let stride = usize::try_from(stride).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    if stride < minimum
        || stride > HYPER_NATIVE_EXTENSIBLE_RECORD_MAX_BYTES as usize
        || stride % 8 != 0
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    Ok(stride)
}

/// Write a supported prefix and clear the unknown tail without proportional scratch.
pub(super) fn copy_strided_record(
    services: &impl UserMemoryServices,
    destination: UserSlice,
    index: usize,
    stride: usize,
    record: &[u8],
) -> Result<(), HyperNativeStatus> {
    let at = index
        .checked_mul(stride)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let end = at.checked_add(stride).ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    if end as u64 > destination.length() {
        return Err(HYPER_NATIVE_STATUS_INTERNAL);
    }
    let base = destination
        .base()
        .checked_add(at as u64)
        .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
    let prefix = record.len().min(stride);
    let output = UserSlice::new(base, prefix as u64).map_err(status_from_address_error)?;
    services
        .copy_to_user(output, &record[..prefix])
        .map_err(status_from_process_error)?;
    let zero = [0u8; 64];
    let mut done = prefix;
    while done < stride {
        let count = (stride - done).min(zero.len());
        let address = base
            .checked_add(done as u64)
            .ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
        let output = UserSlice::new(address, count as u64).map_err(status_from_address_error)?;
        services
            .copy_to_user(output, &zero[..count])
            .map_err(status_from_process_error)?;
        done += count;
    }
    Ok(())
}

pub(super) fn copy_encoded_page<T: Copy, const N: usize, const R: usize>(
    services: &impl UserMemoryServices,
    destination: UserSlice,
    page: &Page<T, N>,
    encode: impl Fn(T) -> [u8; R],
) -> Result<(usize, u64), HyperNativeStatus> {
    let stride = destination.length() as usize / N;
    for (index, entry) in page.entries().enumerate() {
        copy_strided_record(services, destination, index, stride, &encode(*entry))?;
    }
    Ok((page.len(), page.next()))
}

pub(super) fn write_u32(record: &mut [u8], offset: usize, value: u32) {
    record[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

pub(super) fn write_u64(record: &mut [u8], offset: usize, value: u64) {
    record[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

pub(super) fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
