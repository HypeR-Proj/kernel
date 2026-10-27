// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native handles syscall validation.

use crate::abi::native::Arguments;
use crate::abi::native::services::HandleServices;
use crate::capability::InvocationReceiver;
use crate::capability::Rights;
use hyper::abi::native::{
    HYPER_NATIVE_HANDLE_INFO_MIN_SIZE, HYPER_NATIVE_OBJECT_BASIC_INFO_MIN_SIZE, NativeResult,
};
use {
    crate::abi::native::status::handle_result, crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_process_error, crate::abi::native::status::status_only,
};
use {
    crate::abi::native::wire::HANDLE_INFO_SIZE, crate::abi::native::wire::OBJECT_BASIC_INFO_SIZE,
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::encode_handle_info,
    crate::abi::native::wire::encode_object_basic_info,
    crate::abi::native::wire::prepare_info_request,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_handle_close(
    services: &impl HandleServices,
    receiver: &InvocationReceiver,
    _arguments: &Arguments,
) -> NativeResult {
    let result = services
        .close_handle(receiver)
        .map_err(status_from_process_error);
    status_only(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_handle_duplicate(
    services: &impl HandleServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    let result = Rights::from_bits(arguments[0])
        .ok_or(hyper::abi::native::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
        .and_then(|rights| {
            services
                .duplicate_handle(receiver, rights)
                .map_err(status_from_process_error)
        });
    handle_result(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_handle_replace(
    services: &impl HandleServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    let result = Rights::from_bits(arguments[0])
        .ok_or(hyper::abi::native::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
        .and_then(|rights| {
            services
                .replace_handle(receiver, rights)
                .map_err(status_from_process_error)
        });
    handle_result(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_handle_get_info(
    services: &(impl HandleServices + crate::abi::native::services::UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_HANDLE_INFO_MIN_SIZE,
        HANDLE_INFO_SIZE,
    )
    .and_then(|request| {
        let value = receiver;
        let info = services
            .handle_info(value, Rights::NONE)
            .map_err(status_from_process_error)?;
        let record = encode_handle_info(info);
        copy_info_record(services, request, &record)
    });
    info_result(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_object_get_basic_info(
    services: &(impl HandleServices + crate::abi::native::services::UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_OBJECT_BASIC_INFO_MIN_SIZE,
        OBJECT_BASIC_INFO_SIZE,
    )
    .and_then(|request| {
        let value = receiver;
        let info = services
            .handle_info(value, Rights::INSPECT)
            .map_err(status_from_process_error)?;
        let record = encode_object_basic_info(info);
        copy_info_record(services, request, &record)
    });
    info_result(result)
}
