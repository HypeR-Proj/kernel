// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native console syscall validation.

use crate::abi::native::Arguments;
use crate::capability::InvocationReceiver;
use {crate::abi::native::services::ConsoleServices, crate::abi::native::services::NativeAction};
use {
    crate::abi::native::status::console_io_result,
    crate::abi::native::status::status_from_console_service_error,
};
use {crate::abi::native::wire::parse_console_io, crate::abi::native::wire::require_zero};

#[inline(never)]
pub(in crate::abi::native) fn sys_console_discard_input(
    services: &impl ConsoleServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = require_zero(arguments).and_then(|()| {
        services
            .discard_console_input(receiver)
            .map_err(status_from_console_service_error)
    });
    NativeAction::Return(console_io_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_console_read(
    services: &impl ConsoleServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_console_io(arguments).and_then(|bytes| {
        services
            .read_console(receiver, bytes)
            .map_err(status_from_console_service_error)
    });
    NativeAction::Return(console_io_result(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_console_write(
    services: &impl ConsoleServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = parse_console_io(arguments).and_then(|bytes| {
        services
            .write_console(receiver, bytes)
            .map_err(status_from_console_service_error)
    });
    NativeAction::Return(console_io_result(result))
}
