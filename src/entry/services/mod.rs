// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concrete Native service adapters over borrowed Process and Thread authority.
//!
//! The parent owns the shared syscall context and basic handle/user-copy calls;
//! each child implements a capability domain without extending the borrow.

mod affinity;
mod console;
mod device;
mod guest_io;
mod inspect;
mod ipc;
mod memory;
mod object;
mod process_builder;
mod retention;
mod task;
mod vfs;
mod vfs_namespace;
mod vm;

use crate::capability::InvocationReceiver;

use crate::mm::user_space::UserSlice;
use {crate::abi::native::HandleServices, crate::abi::native::UserMemoryServices};
use {crate::capability::HandleInfo, crate::capability::HandleValue, crate::capability::Rights};
use {crate::process::Process, crate::process::ProcessError, crate::process::UserThread};

/// Borrowed syscall authority; contains no machine-run or return-token state.
pub(super) struct ProcessServices<'process> {
    process: &'process Process,
    thread: &'process UserThread,
}

impl<'process> ProcessServices<'process> {
    pub(super) fn new(process: &'process Process, thread: &'process UserThread) -> Self {
        Self { process, thread }
    }
}

impl UserMemoryServices for ProcessServices<'_> {
    fn copy_to_user(&self, destination: UserSlice, source: &[u8]) -> Result<(), ProcessError> {
        self.process.copy_to_user(destination, source)
    }

    fn copy_from_user(
        &self,
        source: UserSlice,
        destination: &mut [u8],
    ) -> Result<(), ProcessError> {
        self.process.copy_from_user(source, destination)
    }
}

impl HandleServices for ProcessServices<'_> {
    fn resolve_invocation(&self, value: HandleValue) -> Result<InvocationReceiver, ProcessError> {
        self.process.resolve_invocation(value)
    }

    fn validate_invocation_context(
        &self,
        receiver: &InvocationReceiver,
    ) -> Result<(), ProcessError> {
        self.process.validate_invocation_context(receiver)
    }

    fn close_handle(&self, value: &InvocationReceiver) -> Result<(), ProcessError> {
        self.process.close_handle(value.handle())
    }

    fn handle_info(
        &self,
        value: &InvocationReceiver,
        required_rights: Rights,
    ) -> Result<HandleInfo, ProcessError> {
        value
            .require(required_rights)
            .map(|()| value.info())
            .map_err(Into::into)
    }

    fn duplicate_handle(
        &self,
        value: &InvocationReceiver,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        self.process.duplicate_handle(value.handle(), rights)
    }
    fn replace_handle(
        &self,
        value: &InvocationReceiver,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        self.process.replace_handle(value.handle(), rights)
    }
}
