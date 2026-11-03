// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Console transfer bounds.

use super::optional_user_slice;
use crate::abi::native::Arguments;
use crate::mm::user_space::UserSlice;
use hyper::abi::native::{
    HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES, HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    HyperNativeStatus,
};

pub(in crate::abi::native) fn parse_console_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[0] != 0 || arguments[2] > HYPER_NATIVE_CONSOLE_MAX_TRANSFER_BYTES {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[1], arguments[2])
}
