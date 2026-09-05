// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral validation of the initial Native syscall dispatcher.

pub(crate) fn run() -> Result<(), crate::kernel::abi::native::SelfTestError> {
    crate::kernel::abi::native::run_self_test()
}
