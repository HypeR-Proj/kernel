// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native task service calls under borrowed Process authority.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use super::affinity::AffinityInputError;
use {crate::abi::native::ObjectServiceError, crate::abi::native::TaskServices};
use {crate::capability::HandleValue, crate::capability::Rights};
use {crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice};
use {
    crate::process::ProcessError, crate::process::ProcessObject, crate::process::ProcessSnapshot,
    crate::process::TerminalReason, crate::process::UserThread, crate::process::UserThreadPhase,
};

impl TaskServices for ProcessServices<'_> {
    fn get_scheduling(
        &self,
        receiver: &InvocationReceiver,
    ) -> Result<crate::task::policy::SchedulingParameters, ObjectServiceError> {
        use crate::object::ObjectKind;
        receiver
            .require(Rights::INSPECT)
            .map_err(ProcessError::from)?;
        if receiver.kind() == ObjectKind::VIRTUAL_CPU {
            return receiver
                .resolve::<crate::vm::objects::VirtualCpuObject>(Rights::INSPECT)
                .map_err(ProcessError::from)?
                .object()
                .scheduling()
                .map_err(scheduling_vm_error);
        }
        let thread = if receiver.kind() == ObjectKind::EXECUTION_CONTEXT {
            self.thread.clone()
        } else {
            UserThread::resolve_receiver(receiver, Rights::INSPECT).map_err(ProcessError::from)?
        };
        let id = thread
            .scheduler_id()
            .ok_or(ObjectServiceError::InvalidInput)?;
        crate::task::scheduler::scheduling_snapshot(id)
            .map_err(ProcessError::from)
            .map_err(Into::into)
    }

    fn set_scheduling(
        &self,
        receiver: &InvocationReceiver,
        parameters: crate::task::policy::SchedulingParameters,
        delegated: Option<Rights>,
    ) -> Result<(), ObjectServiceError> {
        use crate::object::ObjectKind;
        use crate::task::scheduling_authority::SchedulingAuthority;
        let authority = SchedulingAuthority::new(receiver.info().rights);
        match receiver.kind() {
            ObjectKind::PROCESS_BUILDER => {
                let builder = receiver
                    .resolve::<crate::process::ProcessBuilder>(Rights::NONE)
                    .map_err(ProcessError::from)?;
                builder
                    .object()
                    .set_scheduling(
                        parameters,
                        authority,
                        delegated.ok_or(ObjectServiceError::InvalidInput)?,
                    )
                    .map_err(|error| match error {
                        crate::process::ProcessBuilderError::Process(error) => {
                            ObjectServiceError::Process(error)
                        }
                        _ => ObjectServiceError::InvalidInput,
                    })
            }
            ObjectKind::VIRTUAL_CPU => receiver
                .resolve::<crate::vm::objects::VirtualCpuObject>(Rights::NONE)
                .map_err(ProcessError::from)?
                .object()
                .set_scheduling(parameters, authority)
                .map_err(scheduling_vm_error),
            _ => {
                let thread = if receiver.kind() == ObjectKind::EXECUTION_CONTEXT {
                    self.thread.clone()
                } else {
                    UserThread::resolve_receiver(receiver, Rights::NONE)
                        .map_err(ProcessError::from)?
                };
                let id = thread
                    .scheduler_id()
                    .ok_or(ObjectServiceError::InvalidInput)?;
                let limits = thread
                    .scheduling_limits()
                    .ok_or(ObjectServiceError::InvalidInput)?;
                crate::task::scheduler::configure_scheduling(id, parameters, authority, limits)
                    .map_err(ProcessError::from)
                    .map_err(Into::into)
            }
        }
    }

    fn set_current_thread_name(&self, name: &str) -> Result<(), ProcessError> {
        let id = self.thread.scheduler_id().ok_or(ProcessError::Scheduler(
            crate::task::scheduler::Error::ThreadNotFound,
        ))?;
        crate::task::scheduler::set_thread_name(id, name).map_err(ProcessError::from)
    }
    fn create_thread(
        &self,
        scheduling_rights: Rights,
        entry: u64,
        stack: u64,
        tls: u64,
        argument: u64,
        affinity_words: Option<UserSlice>,
        affinity_word_count: usize,
    ) -> Result<HandleValue, ObjectServiceError> {
        let start = crate::process::UserThreadStart::try_new(
            UserAddress::new(entry),
            UserAddress::new(stack),
            UserAddress::new(tls),
        )
        .map_err(|_| ObjectServiceError::InvalidInput)?
        .with_argument(argument);
        let affinity = if affinity_words.is_none() && affinity_word_count == 0 {
            let caller = self
                .thread
                .scheduler_id()
                .ok_or(ObjectServiceError::InvalidInput)?;
            crate::task::scheduler::thread_placement(caller)
                .map_err(ProcessError::from)?
                .1
        } else {
            let affinity = self
                .copy_affinity(affinity_words, affinity_word_count)
                .map_err(|error| match error {
                    AffinityInputError::Invalid => ObjectServiceError::InvalidInput,
                    AffinityInputError::Memory(error) => ObjectServiceError::Process(error),
                })?;
            crate::task::scheduler::validate_affinity(affinity)
                .map_err(|_| ObjectServiceError::InvalidInput)?;
            affinity
        };
        let process = self.process;
        let caller = self
            .thread
            .scheduler_id()
            .ok_or(ObjectServiceError::InvalidInput)?;
        let scheduling =
            crate::task::scheduler::scheduling_snapshot(caller).map_err(ProcessError::from)?;
        let thread = process.create_user_thread_with_scheduling(
            "native-worker",
            start,
            affinity,
            scheduling,
        )?;
        let rights = Rights::DUPLICATE
            .union(Rights::WAIT)
            .union(Rights::INSPECT)
            .union(Rights::START)
            .union(Rights::REQUEST_STOP)
            .union(Rights::TRANSFER)
            .union(Rights::SCHED_LOWER)
            .union(scheduling_rights.intersection(process.scheduling().delegated));
        match process.publish_thread_handle(&thread, rights) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if let Some(id) = thread.scheduler_id() {
                    crate::task::scheduler::request_user_thread_stop(id, TerminalReason::Requested)
                        .map_err(ProcessError::from)?;
                }
                Err(error.into())
            }
        }
    }
    fn start_thread(&self, value: &InvocationReceiver) -> Result<(), ProcessError> {
        let thread = UserThread::resolve_receiver(value, Rights::START)?;
        thread.ready()?;
        Ok(())
    }
    fn stop_thread(&self, value: &InvocationReceiver) -> Result<(), ProcessError> {
        let thread = UserThread::resolve_receiver(value, Rights::REQUEST_STOP)?;
        if thread.snapshot().phase == UserThreadPhase::Detached {
            return Ok(());
        }
        if let Some(id) = thread.scheduler_id() {
            match crate::task::scheduler::request_user_thread_stop(id, TerminalReason::Requested) {
                Ok(()) => {}
                Err(crate::task::scheduler::Error::ThreadNotFound)
                    if thread.snapshot().phase == UserThreadPhase::Detached => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    fn atomic_wait(
        &self,
        address: u64,
        expected: u32,
        deadline: u64,
    ) -> Result<crate::task::WaitOutcome, ObjectServiceError> {
        crate::process::atomic_wait::wait(self.process, address, expected, deadline, || {
            self.thread.snapshot().phase == UserThreadPhase::StopRequested
        })
        .map_err(atomic_wait_error)
    }
    fn atomic_wake(&self, address: u64, count: u32) -> Result<u64, ObjectServiceError> {
        crate::process::atomic_wait::wake(self.process, address, count).map_err(atomic_wait_error)
    }
    fn sleep_thread(&self, deadline: u64) -> Result<crate::task::WaitOutcome, ObjectServiceError> {
        crate::process::atomic_wait::sleep(self.process, deadline, || {
            self.thread.snapshot().phase == UserThreadPhase::StopRequested
        })
        .map_err(atomic_wait_error)
    }

    fn process_info(&self, process: &InvocationReceiver) -> Result<ProcessSnapshot, ProcessError> {
        Ok(process
            .resolve::<ProcessObject>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)?
            .object()
            .snapshot())
    }

    fn derive_process_observer(
        &self,
        process: &InvocationReceiver,
    ) -> Result<HandleValue, ProcessError> {
        self.process.derive_process_observer(process.handle())
    }

    fn request_task_group_stop(&self, group: &InvocationReceiver) -> Result<(), ProcessError> {
        let group = group
            .resolve::<crate::process::TaskGroupObject>(Rights::REQUEST_STOP)
            .map_err(crate::process::ProcessError::from)?;
        group.object().group().request_stop()?;
        Ok(())
    }

    fn request_process_stop(&self, process: &InvocationReceiver) -> Result<(), ProcessError> {
        let process = process
            .resolve::<ProcessObject>(Rights::REQUEST_STOP)
            .map_err(crate::process::ProcessError::from)?;
        process.object().request_stop(TerminalReason::Requested);
        Ok(())
    }
}

fn atomic_wait_error(error: crate::process::atomic_wait::Error) -> ObjectServiceError {
    use crate::process::atomic_wait::Error;
    match error {
        Error::Process(error) => ObjectServiceError::Process(error),
        Error::Wait(error) => ObjectServiceError::Wait(error),
        Error::InvalidInput => ObjectServiceError::InvalidInput,
    }
}

fn scheduling_vm_error(error: crate::vm::objects::Error) -> ObjectServiceError {
    match error {
        crate::vm::objects::Error::Scheduler(error) => {
            ObjectServiceError::Process(ProcessError::Scheduler(error))
        }
        crate::vm::objects::Error::BadState => ObjectServiceError::Process(
            ProcessError::Scheduler(crate::task::scheduler::Error::InvalidThreadState),
        ),
        _ => ObjectServiceError::InvalidInput,
    }
}
