// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Console transfer contract and its domain errors.

use crate::capability::InvocationReceiver;
use crate::mm::user_space::UserSlice;
use crate::process::ProcessError;

#[derive(Debug)]
pub(crate) enum ConsoleServiceError {
    Process(ProcessError),
    Io(crate::device::console::IoError),
}

impl From<ProcessError> for ConsoleServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<crate::device::console::IoError> for ConsoleServiceError {
    fn from(error: crate::device::console::IoError) -> Self {
        Self::Io(error)
    }
}

pub(crate) trait ConsoleServices {
    fn discard_console_input(
        &self,
        console: &InvocationReceiver,
    ) -> Result<usize, ConsoleServiceError>;
    fn read_console(
        &self,
        console: &InvocationReceiver,
        bytes: Option<UserSlice>,
    ) -> Result<usize, ConsoleServiceError>;
    fn write_console(
        &self,
        console: &InvocationReceiver,
        bytes: Option<UserSlice>,
    ) -> Result<usize, ConsoleServiceError>;
}
