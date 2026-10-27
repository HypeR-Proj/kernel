// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native system syscall validation.

use crate::abi::native::Arguments;
use crate::abi::native::services::NativeAction;
use crate::abi::native::wire::require_zero;
use hyper::abi::native as abi;
use hyper::abi::native::{
    HYPER_NATIVE_ABI_REVISION, HYPER_NATIVE_FEATURE_CORE, HYPER_NATIVE_STATUS_INTERNAL,
    HYPER_NATIVE_STATUS_NOT_SUPPORTED, NativeResult,
};
use {crate::abi::native::status::failure, crate::abi::native::status::success};

#[inline(never)]
pub(in crate::abi::native) fn sys_abi_query(_arguments: &Arguments) -> NativeResult {
    success([HYPER_NATIVE_ABI_REVISION, HYPER_NATIVE_FEATURE_CORE])
}

#[inline(never)]
pub(in crate::abi::native) fn sys_system_config(arguments: &Arguments) -> NativeResult {
    if let Err(status) = require_zero(&arguments[1..]) {
        return failure(status);
    }
    match arguments[0] {
        hyper::abi::native::HYPER_NATIVE_SYSTEM_CONFIG_PAGE_SIZE => {
            success([hyper::mm::PAGE_SIZE, 0])
        }
        hyper::abi::native::HYPER_NATIVE_SYSTEM_CONFIG_APPLICATION_ADDRESS_LIMIT => {
            match crate::hal::user::address_space_plan() {
                Ok(plan) => success([plan.application_limit(), 0]),
                Err(_) => failure(HYPER_NATIVE_STATUS_INTERNAL),
            }
        }
        _ => failure(HYPER_NATIVE_STATUS_NOT_SUPPORTED),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_clock_get_monotonic(arguments: &Arguments) -> NativeResult {
    let result = require_zero(arguments).and_then(|()| {
        crate::time::monotonic_nanoseconds().map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)
    });
    match result {
        Ok(nanoseconds) => success([nanoseconds, 0]),
        Err(status) => failure(status),
    }
}

#[inline(never)]
pub(in crate::abi::native) fn sys_clock_get_realtime(arguments: &Arguments) -> NativeAction {
    let result = (|| {
        require_zero(arguments)?;
        crate::time::realtime().ok_or(abi::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    })();
    NativeAction::Return(match result {
        Ok(time) => success([time.seconds() as u64, u64::from(time.nanoseconds())]),
        Err(status) => failure(status),
    })
}
