// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime integration of vCPU endpoint publication with scheduler waiting.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Endpoint(crate::vm::WaitSelfTestError),
    Quiescence(super::support::QuiescenceError),
}

pub(super) fn run() -> Result<(), Error> {
    let result = crate::vm::run_wait_self_test().map_err(Error::Endpoint);
    let quiescence = super::support::quiesce_workers().map_err(Error::Quiescence);
    result?;
    quiescence?;
    Ok(())
}
