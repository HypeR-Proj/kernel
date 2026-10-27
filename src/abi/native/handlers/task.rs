// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native task syscall validation.

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use hyper::abi::native::{
    HYPER_NATIVE_PROCESS_INFO_MIN_SIZE, HYPER_NATIVE_STATUS_CANCELLED,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HYPER_NATIVE_STATUS_TIMED_OUT, HyperNativeStatus,
    NativeResult,
};
use {
    crate::abi::native::services::HierarchyServices, crate::abi::native::services::NativeAction,
    crate::abi::native::services::ObjectServices, crate::abi::native::services::TaskServices,
};
use {
    crate::abi::native::status::failure, crate::abi::native::status::handle_result,
    crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_hierarchy_error,
    crate::abi::native::status::status_from_object_service_error,
    crate::abi::native::status::status_from_process_error, crate::abi::native::status::status_only,
    crate::abi::native::status::success,
};
use {
    crate::abi::native::wire::PROCESS_INFO_SIZE, crate::abi::native::wire::copy_info_record,
    crate::abi::native::wire::decode_resource_limits,
    crate::abi::native::wire::encode_process_info, crate::abi::native::wire::parse_affinity_words,
    crate::abi::native::wire::parse_handle, crate::abi::native::wire::prepare_info_request,
    crate::abi::native::wire::require_zero,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_set_current_name(
    services: &impl TaskServices,
    arguments: &Arguments,
) -> NativeResult {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let length = arguments[1];
        if length == 0 || length > hyper::abi::native::HYPER_NATIVE_THREAD_NAME_MAX_BYTES {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let input = super::super::wire::optional_user_slice(arguments[0], length)?
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let mut bytes = [0; hyper::abi::native::HYPER_NATIVE_THREAD_NAME_MAX_BYTES as usize];
        let bytes = &mut bytes[..length as usize];
        services
            .copy_from_user(input, bytes)
            .map_err(status_from_process_error)?;
        let name = core::str::from_utf8(bytes).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if name.chars().any(char::is_control) {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        services
            .set_current_thread_name(name)
            .map_err(status_from_process_error)
    })();
    status_only(result)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_resource_domain_create(
    services: &impl HierarchyServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let parent = receiver;
        let limits = decode_resource_limits(services, arguments)?;
        services
            .create_resource_domain(parent, limits)
            .map_err(status_from_hierarchy_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_group_create(
    services: &impl HierarchyServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        let factory = receiver;
        let domain = parse_handle(arguments[0])?;
        require_zero(&arguments[1..])?;
        services
            .create_task_group(factory, domain)
            .map_err(status_from_hierarchy_error)
    })();
    NativeAction::Return(handle_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_yield() -> NativeAction {
    NativeAction::Yield(success([0, 0]))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_exit(arguments: &Arguments) -> NativeAction {
    NativeAction::ExitThread {
        status: arguments[0] as i64,
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_process_exit(arguments: &Arguments) -> NativeAction {
    NativeAction::ExitProcess {
        status: arguments[0] as i64,
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_process_derive_observer(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(handle_result(require_zero(arguments).and_then(|()| {
        services
            .derive_process_observer(receiver)
            .map_err(status_from_process_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_task_group_request_stop(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(status_only(require_zero(arguments).and_then(|()| {
        services
            .request_task_group_stop(receiver)
            .map_err(status_from_process_error)
    })))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_process_request_stop(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    _arguments: &Arguments,
) -> NativeAction {
    let result = services
        .request_process_stop(receiver)
        .map_err(status_from_process_error);
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_process_get_info(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = prepare_info_request(
        arguments,
        HYPER_NATIVE_PROCESS_INFO_MIN_SIZE,
        PROCESS_INFO_SIZE,
    )
    .and_then(|request| {
        let process = receiver;
        let snapshot = services
            .process_info(process)
            .map_err(status_from_process_error)?;
        let record = encode_process_info(snapshot);
        copy_info_record(services, request, &record)
    });
    NativeAction::Return(info_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_create(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    let affinity = if arguments[4] == 0 && arguments[5] == 0 {
        Ok((None, 0))
    } else {
        parse_affinity_words(arguments[4], arguments[5])
    };
    handle_result(affinity.and_then(|(words, count)| {
        services
            .create_thread(
                receiver.info().rights,
                arguments[0],
                arguments[1],
                arguments[2],
                arguments[3],
                words,
                count,
            )
            .map_err(status_from_object_service_error)
    }))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_start(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    status_only(require_zero(arguments).and_then(|()| {
        services
            .start_thread(receiver)
            .map_err(status_from_process_error)
    }))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_request_stop(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    status_only(require_zero(arguments).and_then(|()| {
        services
            .stop_thread(receiver)
            .map_err(status_from_process_error)
    }))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_atomic_wait(
    services: &impl TaskServices,
    arguments: &Arguments,
) -> NativeResult {
    let result = require_zero(&arguments[3..]).and_then(|()| {
        let expected =
            u32::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .atomic_wait(arguments[0], expected, arguments[2])
            .map_err(status_from_object_service_error)
    });
    wait_status(result, false)
}

#[inline(never)]
pub(in crate::abi::native) fn sys_atomic_wake(
    services: &impl TaskServices,
    arguments: &Arguments,
) -> NativeResult {
    match require_zero(&arguments[2..]).and_then(|()| {
        let count =
            u32::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .atomic_wake(arguments[0], count)
            .map_err(status_from_object_service_error)
    }) {
        Ok(count) => success([count, 0]),
        Err(status) => failure(status),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_thread_sleep(
    services: &impl TaskServices,
    arguments: &Arguments,
) -> NativeResult {
    wait_status(
        require_zero(&arguments[1..]).and_then(|()| {
            services
                .sleep_thread(arguments[0])
                .map_err(status_from_object_service_error)
        }),
        true,
    )
}

fn wait_status(
    result: Result<crate::task::WaitOutcome, HyperNativeStatus>,
    sleep: bool,
) -> NativeResult {
    use crate::task::WaitOutcome;
    match result {
        Ok(WaitOutcome::Notified) => success([0, 0]),
        Ok(WaitOutcome::TimedOut) if sleep => success([0, 0]),
        Ok(WaitOutcome::TimedOut) => failure(HYPER_NATIVE_STATUS_TIMED_OUT),
        Ok(WaitOutcome::Cancelled) => failure(HYPER_NATIVE_STATUS_CANCELLED),
        Err(status) => failure(status),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_process_get_current_id(
    services: &impl ObjectServices,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(if arguments.iter().any(|value| *value != 0) {
        failure(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
    } else {
        success([services.current_process_id(), 0])
    })
}
