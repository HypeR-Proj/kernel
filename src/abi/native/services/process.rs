// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process construction, task hierarchy, and retained evidence contracts.

use super::memory::UserMemoryServices;
use super::object::ObjectServiceError;
use crate::capability::{HandleValue, InvocationReceiver, Rights};
use crate::mm::user_space::UserSlice;
use crate::process::{ChildProcessStartError, ProcessBuilderError, ProcessError, ProcessSnapshot};

#[derive(Debug)]
pub(crate) enum ProcessBuilderServiceError {
    InvalidInput,
    Process(ProcessError),
    Builder(ProcessBuilderError<()>),
    Start(ProcessBuilderError<ChildProcessStartError>),
}

impl From<ProcessError> for ProcessBuilderServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<ProcessBuilderError<()>> for ProcessBuilderServiceError {
    fn from(error: ProcessBuilderError<()>) -> Self {
        Self::Builder(error)
    }
}

pub(crate) trait HierarchyServices: UserMemoryServices {
    fn create_resource_domain(
        &self,
        parent: &InvocationReceiver,
        limits: crate::accounting::ResourceLimits,
    ) -> Result<HandleValue, crate::process::hierarchy::Error>;
    fn create_task_group(
        &self,
        factory: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::process::hierarchy::Error>;
}

pub(crate) trait ProcessBuilderServices {
    fn create_process_builder(
        &self,
        factory: &InvocationReceiver,
        group: HandleValue,
        domain: HandleValue,
        executable: HandleValue,
        loader: HandleValue,
    ) -> Result<HandleValue, ProcessBuilderServiceError>;
    fn set_process_builder_name(
        &self,
        builder: &InvocationReceiver,
        name: Option<UserSlice>,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn set_process_builder_data(
        &self,
        builder: &InvocationReceiver,
        data: Option<UserSlice>,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn set_process_builder_affinity(
        &self,
        builder: &InvocationReceiver,
        words: Option<UserSlice>,
        word_count: usize,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn set_process_builder_memory_lock(
        &self,
        builder: &InvocationReceiver,
        enabled: bool,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn add_process_builder_handle(
        &self,
        builder: &InvocationReceiver,
        source: HandleValue,
        purpose: u32,
        expected_kind: crate::object::ObjectKind,
        requested_rights: Option<Rights>,
        operation: crate::capability::HandleTransferOperation,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn seal_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<(), ProcessBuilderServiceError>;
    fn start_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<[HandleValue; 2], ProcessBuilderServiceError>;
    fn abort_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<(), ProcessBuilderServiceError>;
}

pub(crate) trait TaskServices: UserMemoryServices {
    fn get_scheduling(
        &self,
        _receiver: &InvocationReceiver,
    ) -> Result<crate::task::policy::SchedulingParameters, ObjectServiceError> {
        Err(ObjectServiceError::InvalidInput)
    }
    fn set_scheduling(
        &self,
        _receiver: &InvocationReceiver,
        _parameters: crate::task::policy::SchedulingParameters,
        _delegated: Option<Rights>,
    ) -> Result<(), ObjectServiceError> {
        Err(ObjectServiceError::InvalidInput)
    }
    fn set_current_thread_name(&self, name: &str) -> Result<(), ProcessError>;
    #[allow(
        clippy::too_many_arguments,
        reason = "validated Native entry registers plus captured scheduling authority"
    )]
    fn create_thread(
        &self,
        scheduling_rights: Rights,
        entry: u64,
        stack: u64,
        tls: u64,
        argument: u64,
        affinity_words: Option<UserSlice>,
        affinity_word_count: usize,
    ) -> Result<HandleValue, ObjectServiceError>;
    fn start_thread(&self, thread: &InvocationReceiver) -> Result<(), ProcessError>;
    fn stop_thread(&self, thread: &InvocationReceiver) -> Result<(), ProcessError>;
    fn atomic_wait(
        &self,
        address: u64,
        expected: u32,
        deadline: u64,
    ) -> Result<crate::task::WaitOutcome, ObjectServiceError>;
    fn atomic_wake(&self, address: u64, count: u32) -> Result<u64, ObjectServiceError>;
    fn sleep_thread(&self, deadline: u64) -> Result<crate::task::WaitOutcome, ObjectServiceError>;
    fn process_info(&self, process: &InvocationReceiver) -> Result<ProcessSnapshot, ProcessError>;
    fn request_process_stop(&self, process: &InvocationReceiver) -> Result<(), ProcessError>;
    fn derive_process_observer(
        &self,
        process: &InvocationReceiver,
    ) -> Result<HandleValue, ProcessError>;
    fn request_task_group_stop(&self, group: &InvocationReceiver) -> Result<(), ProcessError>;
}

pub(crate) trait RetentionServices: UserMemoryServices {
    fn retain_process_builder(
        &self,
        _builder: &InvocationReceiver,
        _bytes: u64,
        _timeout: u64,
    ) -> Result<HandleValue, ProcessBuilderServiceError> {
        Err(ProcessBuilderServiceError::InvalidInput)
    }
    fn retention_info(
        &self,
        _handle: &InvocationReceiver,
        _view: bool,
    ) -> Result<crate::process::retention::Info, crate::process::retention::Error> {
        Err(crate::process::retention::Error::Invalid)
    }
    fn retention_open_view(
        &self,
        _handle: &InvocationReceiver,
    ) -> Result<HandleValue, crate::process::retention::Error> {
        Err(crate::process::retention::Error::Invalid)
    }
    fn retention_release(
        &self,
        _handle: &InvocationReceiver,
    ) -> Result<(), crate::process::retention::Error> {
        Err(crate::process::retention::Error::Invalid)
    }
    fn crash_thread(
        &self,
        _handle: &InvocationReceiver,
        _index: usize,
    ) -> Result<(crate::process::retention::ThreadEvidence, usize), crate::process::retention::Error>
    {
        Err(crate::process::retention::Error::Invalid)
    }
    fn crash_mapping(
        &self,
        _handle: &InvocationReceiver,
        _index: usize,
    ) -> Result<(crate::mm::user_space::CrashMapping, usize), crate::process::retention::Error>
    {
        Err(crate::process::retention::Error::Invalid)
    }
    fn crash_handle(
        &self,
        _handle: &InvocationReceiver,
        _index: usize,
    ) -> Result<(crate::capability::HandleSnapshot, usize), crate::process::retention::Error> {
        Err(crate::process::retention::Error::Invalid)
    }
    fn crash_read(
        &self,
        _handle: &InvocationReceiver,
        _address: u64,
        _output: UserSlice,
    ) -> Result<usize, crate::process::retention::Error> {
        Err(crate::process::retention::Error::Invalid)
    }
}
