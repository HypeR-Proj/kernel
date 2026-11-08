// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability handles, object signals, and wait registration contracts.

use crate::capability::{HandleInfo, HandleValue, InvocationReceiver, Rights};
use crate::mm::user_space::UserSlice;
use crate::object::{EventError, ObjectWaitError, SignalWaitManyOutcome, SignalWaitOutcome};
use crate::process::ProcessError;

#[derive(Debug)]
pub(crate) enum ObjectServiceError {
    InvalidInput,
    Process(ProcessError),
    Event(EventError),
    Wait(ObjectWaitError),
    WaitSet(crate::object::WaitSetError),
}

impl From<ProcessError> for ObjectServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<EventError> for ObjectServiceError {
    fn from(error: EventError) -> Self {
        Self::Event(error)
    }
}

impl From<ObjectWaitError> for ObjectServiceError {
    fn from(error: ObjectWaitError) -> Self {
        Self::Wait(error)
    }
}

pub(crate) trait HandleServices {
    fn resolve_invocation(&self, value: HandleValue) -> Result<InvocationReceiver, ProcessError>;
    fn validate_invocation_context(
        &self,
        receiver: &InvocationReceiver,
    ) -> Result<(), ProcessError>;
    fn close_handle(&self, value: &InvocationReceiver) -> Result<(), ProcessError>;
    fn handle_info(
        &self,
        value: &InvocationReceiver,
        required_rights: Rights,
    ) -> Result<HandleInfo, ProcessError>;
    fn duplicate_handle(
        &self,
        value: &InvocationReceiver,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError>;
    fn replace_handle(
        &self,
        value: &InvocationReceiver,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError>;
}

impl From<crate::object::WaitSetError> for ObjectServiceError {
    fn from(error: crate::object::WaitSetError) -> Self {
        Self::WaitSet(error)
    }
}

pub(crate) trait ObjectServices {
    #[cfg(feature = "kernel-trace")]
    fn trace_thread_id(&self) -> u64 {
        0
    }
    fn current_process_id(&self) -> u64;
    fn wait_set_create(&self, capacity: usize) -> Result<HandleValue, ObjectServiceError>;
    fn wait_set_add(
        &self,
        set: &InvocationReceiver,
        source: HandleValue,
        signals: u64,
    ) -> Result<u64, ObjectServiceError>;
    fn wait_set_rearm(
        &self,
        set: &InvocationReceiver,
        registration: u64,
    ) -> Result<(), ObjectServiceError>;
    fn wait_set_remove(
        &self,
        set: &InvocationReceiver,
        registration: u64,
    ) -> Result<(), ObjectServiceError>;
    fn wait_set_wait(
        &self,
        set: &InvocationReceiver,
        deadline: u64,
        output: UserSlice,
    ) -> Result<(), ObjectServiceError>;

    fn wait_set_wait_batch(
        &self,
        set: &InvocationReceiver,
        deadline: u64,
        output: UserSlice,
        capacity: usize,
        stride: usize,
    ) -> Result<usize, ObjectServiceError>;

    fn create_event(&self) -> Result<HandleValue, ObjectServiceError>;
    fn signal_event(
        &self,
        value: &InvocationReceiver,
        clear: u64,
        set: u64,
    ) -> Result<(), ObjectServiceError>;
    fn wait_one(
        &self,
        value: &InvocationReceiver,
        requested: u64,
        deadline: u64,
    ) -> Result<SignalWaitOutcome, ObjectServiceError>;
    fn wait_many(
        &self,
        items: UserSlice,
        item_count: usize,
        deadline: u64,
    ) -> Result<SignalWaitManyOutcome, ObjectServiceError>;
}
