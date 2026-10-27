// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native console service calls under borrowed Process authority.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use crate::capability::Rights;
use crate::mm::user_space::UserSlice;
use crate::process::ProcessError;
use {crate::abi::native::ConsoleServiceError, crate::abi::native::ConsoleServices};

impl ConsoleServices for ProcessServices<'_> {
    fn discard_console_input(
        &self,
        value: &InvocationReceiver,
    ) -> Result<usize, ConsoleServiceError> {
        let console = value
            .resolve::<crate::device::console::SystemConsole>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?;
        Ok(console.object().discard_input()?)
    }

    fn read_console(
        &self,
        value: &InvocationReceiver,
        destination: Option<UserSlice>,
    ) -> Result<usize, ConsoleServiceError> {
        let console = value
            .resolve::<crate::device::console::SystemConsole>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?;
        let Some(destination) = destination else {
            return Ok(0);
        };
        let capacity =
            usize::try_from(destination.length()).map_err(|_| ProcessError::Allocation)?;
        let claim = console.object().claim_read(capacity)?;
        let actual = claim.bytes().len();
        let actual_bytes = u64::try_from(actual).map_err(|_| ProcessError::Allocation)?;
        let destination = UserSlice::new(destination.base(), actual_bytes)
            .map_err(|error| ProcessError::UserMemory(error.into()))?;
        let write = self.process.reserve_user_write(destination)?;
        write
            .copy_from(claim.bytes())
            .map_err(ProcessError::UserMemory)?;
        write.complete();
        claim.commit();
        Ok(actual)
    }

    fn write_console(
        &self,
        value: &InvocationReceiver,
        source: Option<UserSlice>,
    ) -> Result<usize, ConsoleServiceError> {
        let console = value
            .resolve::<crate::device::console::SystemConsole>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        let Some(source) = source else {
            return Ok(0);
        };
        let length = usize::try_from(source.length())
            .map_err(|_| ProcessError::Allocation)?
            .min(crate::device::console::TRANSFER_BATCH_BYTES);
        let length_bytes = u64::try_from(length).map_err(|_| ProcessError::Allocation)?;
        let source = UserSlice::new(source.base(), length_bytes)
            .map_err(|error| ProcessError::UserMemory(error.into()))?;
        let mut bytes = [0; crate::device::console::TRANSFER_BATCH_BYTES];
        self.process.copy_from_user(source, &mut bytes[..length])?;
        Ok(console.object().try_write(&bytes[..length])?)
    }
}
