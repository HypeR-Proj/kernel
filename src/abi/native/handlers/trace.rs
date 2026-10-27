// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded trace buffers are allocated off-stack. Privileged invocation admission
//! precedes payload access; no user-memory copy holds a recorder lock.
use crate::capability::InvocationReceiver;
use alloc::vec::Vec;
use hyper::abi::native::*;
use {
    crate::abi::native::Arguments, crate::abi::native::services::NativeAction,
    crate::abi::native::services::SystemInspectServices, crate::abi::native::status,
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::optional_user_slice,
    crate::abi::native::wire::prepare_info_request,
};

// The fixed wire encoder must change atomically with the schema layouts.
const _: () = {
    assert!(core::mem::size_of::<HyperNativeTraceConfig>() == 56);
    assert!(core::mem::size_of::<HyperNativeTraceInfo>() == 160);
    assert!(core::mem::size_of::<HyperNativeTraceThread>() == 72);
    assert!(core::mem::size_of::<HyperNativeTraceThreadDetails>() == 80);
    assert!(core::mem::size_of::<HyperNativeTraceRecord>() == 328);
    assert!(core::mem::size_of::<HyperNativeTraceBatch>() == 2656);
};

#[inline(never)]
pub(in crate::abi::native) fn sys_cpu_inspector_trace_start(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    NativeAction::Return(status::info_result((|| {
        let input =
            optional_user_slice(args[0], 56)?.ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let mut bytes = [0; 56];
        services
            .copy_from_user(input, &mut bytes)
            .map_err(status::status_from_process_error)?;
        services.trace_start(receiver, &bytes)
    })()))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_cpu_inspector_trace_stop(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    NativeAction::Return(status::status_only(services.trace_stop(receiver, args[0])))
}
#[inline(never)]
pub(in crate::abi::native) fn sys_cpu_inspector_trace_read(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    NativeAction::Return(status::info_result((|| {
        let size = match args[1] {
            u64::MAX => 160,
            x if x == u64::MAX - 1 => 72,
            x if x == u64::MAX - 2 => 80,
            _ => 2656,
        };
        let request = prepare_info_request(&[args[3], args[4], 0, 0, 0, 0], size, size)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| HYPER_NATIVE_STATUS_NO_MEMORY)?;
        bytes.resize(size, 0);
        let written = services.trace_read(receiver, args[0], args[1], args[2], &mut bytes)?;
        copy_info_record(services, request, &bytes[..written])
    })()))
}
