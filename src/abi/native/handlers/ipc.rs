// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native ipc syscall validation.

use crate::capability::handle_wire;

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use hyper::abi::native::{
    HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL, HYPER_NATIVE_STATUS_INTERNAL,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HyperNativeStatus, NativeOperation, NativeResult,
};
use {crate::abi::native::services::IpcServices, crate::abi::native::services::NativeAction};
use {
    crate::abi::native::status::failure,
    crate::abi::native::status::status_from_byte_channel_service_error,
    crate::abi::native::status::status_from_capability_channel_error,
    crate::abi::native::status::status_from_capability_channel_service_error,
    crate::abi::native::status::status_only, crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::parse_byte_channel_io,
    crate::abi::native::wire::parse_capability_channel_receive,
    crate::abi::native::wire::parse_capability_channel_send,
};
use {
    crate::ipc::ByteChannelReadOutcome, crate::ipc::CapabilityChannelError,
    crate::ipc::CapabilityReceiveOutcome,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_byte_channel_create(
    services: &impl IpcServices,
    arguments: &Arguments,
) -> NativeResult {
    if arguments[0] != 0 {
        return failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    match services
        .create_byte_channel()
        .map_err(status_from_byte_channel_service_error)
    {
        Ok([first, second]) => success([handle_wire::to_abi(first), handle_wire::to_abi(second)]),
        Err(status) => failure(status),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_capability_channel_create(
    services: &impl IpcServices,
    arguments: &Arguments,
) -> NativeResult {
    if arguments[0] != 0 {
        return failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    match services
        .create_capability_channel()
        .map_err(status_from_capability_channel_service_error)
    {
        Ok([first, second]) => success([handle_wire::to_abi(first), handle_wire::to_abi(second)]),
        Err(status) => failure(status),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_byte_channel_write(
    services: &impl IpcServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_byte_channel_io(arguments).and_then(|bytes| {
        services
            .write_byte_channel(receiver, bytes)
            .map_err(status_from_byte_channel_service_error)
    });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_byte_channel_read(
    services: &impl IpcServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_byte_channel_io(arguments).and_then(|bytes| {
        services
            .read_byte_channel(receiver, bytes)
            .map_err(status_from_byte_channel_service_error)
    });
    let result = match result {
        Ok(ByteChannelReadOutcome::Received { bytes }) => success([bytes, 0]),
        Ok(ByteChannelReadOutcome::BufferTooSmall { bytes }) => NativeResult::for_operation(
            NativeOperation::ByteChannelRead,
            HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
            [bytes, 0],
        ),
        Err(status) => failure(status),
    };
    NativeAction::Return(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_capability_channel_try_send(
    services: &impl IpcServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_capability_channel_send(arguments).and_then(|(bytes, dispositions)| {
        services
            .try_send_capability_channel(receiver, bytes, dispositions)
            .map_err(status_from_capability_channel_service_error)
    });
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_capability_channel_receive(
    services: &impl IpcServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result =
        parse_capability_channel_receive(arguments).and_then(|(deadline, bytes, slots)| {
            services
                .receive_capability_channel(receiver, deadline, bytes, slots)
                .map_err(status_from_capability_channel_service_error)
        });
    NativeAction::Return(capability_receive_result(result))
}

pub(in crate::abi::native) fn capability_receive_result(
    result: Result<CapabilityReceiveOutcome, HyperNativeStatus>,
) -> NativeResult {
    match result {
        Ok(CapabilityReceiveOutcome::Delivered(info)) => {
            match (u64::try_from(info.bytes), u64::try_from(info.handles)) {
                (Ok(bytes), Ok(handles)) => success([bytes, handles]),
                _ => failure(HYPER_NATIVE_STATUS_INTERNAL),
            }
        }
        Ok(CapabilityReceiveOutcome::Failed(CapabilityChannelError::BufferTooSmall {
            required_bytes,
            required_handles,
        })) => match (
            u64::try_from(required_bytes),
            u64::try_from(required_handles),
        ) {
            (Ok(bytes), Ok(handles)) => NativeResult::for_operation(
                NativeOperation::CapabilityChannelReceive,
                HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
                [bytes, handles],
            ),
            _ => failure(HYPER_NATIVE_STATUS_INTERNAL),
        },
        Ok(CapabilityReceiveOutcome::Failed(error)) => {
            failure(status_from_capability_channel_error(error))
        }
        Err(status) => failure(status),
    }
}

/// Copies a bounded annotation record; annotations never carry authority.
pub(in crate::abi::native) fn sys_byte_channel_trace_rpc(
    services: &(impl IpcServices + crate::abi::native::services::UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    use crate::abi::native::wire::{copy_extensible_input_record, read_record_u64, require_zero};
    let result = (|| {
        require_zero(&arguments[2..])?;
        let request = copy_extensible_input_record::<64>(services, arguments[0], arguments[1], 64)?;
        if read_record_u64(&request, 48) != 0 || read_record_u64(&request, 56) != 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let values = core::array::from_fn(|i| read_record_u64(&request, i * 8));
        services.trace_byte_channel_rpc(receiver, &values)
    })();
    NativeAction::Return(match result {
        Ok(values) => success(values),
        Err(status) => failure(status),
    })
}
