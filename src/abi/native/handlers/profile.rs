// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! PMU control validates authority before touching sampling state or user memory.
use crate::capability::InvocationReceiver;
use {
    crate::abi::native::Arguments, crate::abi::native::services::NativeAction,
    crate::abi::native::services::SystemInspectServices, crate::abi::native::status,
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::prepare_info_request,
};

pub(in crate::abi::native) fn sys_cpu_inspector_profile_start(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    NativeAction::Return(status::info_result(
        services.profile_start(receiver, args[0], args[1]),
    ))
}
pub(in crate::abi::native) fn sys_cpu_inspector_profile_stop(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    NativeAction::Return(status::status_only(
        services.profile_stop(receiver, args[0]),
    ))
}
pub(in crate::abi::native) fn sys_cpu_inspector_profile_read(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    args: &Arguments,
) -> NativeAction {
    let result = (|| {
        let size = if args[1] == u64::MAX {
            crate::diagnostics::profile::INFO_SIZE
        } else {
            crate::diagnostics::profile::SAMPLE_SIZE
        };
        let request = prepare_info_request(&[args[3], args[4], 0, 0, 0, 0], size, size)?;
        let mut bytes = [0u8; crate::diagnostics::profile::SAMPLE_SIZE];
        let written = services.profile_read(receiver, args[0], args[1], args[2], &mut bytes)?;
        copy_info_record(services, request, &bytes[..written])
    })();
    NativeAction::Return(status::info_result(result))
}
