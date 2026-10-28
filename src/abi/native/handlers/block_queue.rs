// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded asynchronous block calls. Completion status is separate from syscall status.

use crate::abi::native::services::NativeAction;
use crate::block::service::TransferError;
use crate::capability::InvocationReceiver;
use hyper::abi::native::*;
use {crate::abi::native::Arguments, crate::abi::native::GuestIoServices};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::info_result, crate::abi::native::status::status_from_process_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::optional_user_slice,
    crate::abi::native::wire::parse_u32, crate::abi::native::wire::prepare_info_request,
    crate::abi::native::wire::require_zero,
};

fn status(error: TransferError) -> i64 {
    match error {
        TransferError::Capability(error) => status_from_process_error(error),
        TransferError::Device(error) => crate::block::service::queue::completion_status(Err(error)),
        TransferError::Status(status) => status,
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_create(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let capacity = parse_u32(arguments[0])?;
        let inflight = parse_u32(arguments[2])?;
        if capacity == 0
            || u64::from(capacity) > HYPER_NATIVE_BLOCK_QUEUE_CAPACITY_MAX
            || arguments[1] < 512
            || arguments[1] > HYPER_NATIVE_BLOCK_QUEUE_BYTE_LIMIT_MAX
            || inflight == 0
            || u64::from(inflight) > HYPER_NATIVE_BLOCK_QUEUE_INFLIGHT_MAX
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .create_block_queue(receiver, capacity, arguments[1], inflight)
            .map_err(status)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_submit(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let operation = parse_u32(arguments[0])?;
        let count = parse_u32(arguments[1])?;
        if operation > 2
            || arguments[3] > HYPER_NATIVE_BLOCK_QUEUE_FRAME_BYTES
            || (operation == 2 && arguments[1..4].iter().any(|value| *value != 0))
            || (operation != 2
                && (count == 0 || u64::from(count) > HYPER_NATIVE_BLOCK_QUEUE_BATCH_MAX))
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let frame = optional_user_slice(arguments[2], arguments[3])?;
        services
            .submit_block_queue(receiver, operation, count, frame)
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok(ticket) => success([ticket, 0]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_wait(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        services
            .wait_block_queue(receiver, arguments[0], arguments[1])
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok((ticket, status)) => success([ticket, status as u64]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_reap(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        if arguments[0] == 0 || arguments[2] > HYPER_NATIVE_BLOCK_QUEUE_TRANSFER_BYTES {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let output = optional_user_slice(arguments[1], arguments[2])?;
        services
            .reap_block_queue(receiver, arguments[0], output)
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok((status, bytes)) => success([status as u64, bytes as u64]),
        Err(status) => failure(status),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_info(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let output = prepare_info_request(arguments, 128, 128)?;
        let bytes = services.block_queue_info(receiver).map_err(status)?;
        copy_info_record(services, output, &bytes)
    })();
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_wait_space(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        if !arguments[0].is_multiple_of(512)
            || arguments[0] > HYPER_NATIVE_BLOCK_QUEUE_TRANSFER_BYTES
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .wait_block_queue_space(receiver, arguments[0], arguments[1])
            .map_err(status)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_create_window(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(arguments)?;
        services.create_block_queue_window(receiver).map_err(status)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_block_queue_submit_window(
    services: &impl GuestIoServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[4..])?;
        let count = parse_u32(arguments[1])?;
        if arguments[0] == 0
            || count == 0
            || u64::from(count) > HYPER_NATIVE_BLOCK_QUEUE_BATCH_MAX
            || arguments[3] != u64::from(count) * HYPER_NATIVE_BLOCK_QUEUE_WINDOW_RANGE_BYTES
        {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let frame = optional_user_slice(arguments[2], arguments[3])?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .submit_block_queue_window(receiver, arguments[0], count, frame)
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok(ticket) => success([ticket, 0]),
        Err(status) => failure(status),
    })
}
