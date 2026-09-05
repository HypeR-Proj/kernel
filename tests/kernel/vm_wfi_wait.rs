// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime integration of vCPU endpoint publication with scheduler waiting.

pub(super) fn run() -> Result<(), crate::kernel::vm::WaitSelfTestError> {
    crate::kernel::vm::run_wait_self_test()
}
