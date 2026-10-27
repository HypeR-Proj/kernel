// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Construction and launch of the initial Native userspace supervisor.

use core::convert::Infallible;

use crate::capability::HandleError;
use crate::task::scheduler;
use {
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceLimits,
};
use {crate::process::ProcessError, crate::process::TaskGroup, crate::process::TaskGroupError};

mod bootstrap;
mod capabilities;

const INIT_PATH: &str = "/init";
pub(crate) enum Error {
    RootDirectory(crate::fs::vfs::VfsError),
    #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
    ConsoleObject(crate::device::console::ObjectError),
    Handle(HandleError),
    Image(crate::process::LoaderError),
    Inspection(crate::diagnostics::inspect::Error),
    IncompleteThreadPublication,
    Scheduler(scheduler::Error),
    MemoryObject(crate::mm::user_space::MemoryObjectError),
    Object(crate::object::ObjectCreationError),
    Process(ProcessError),
    Resource(ResourceError),
    ResourceObject(crate::accounting::ResourceDomainObjectError),
    Bootstrap(crate::process::bootstrap::Error),
    TaskGroup(TaskGroupError),
    TaskObject(crate::process::TaskObjectError),
    VirtualMachineObject(crate::vm::objects::Error),
    DeviceAssignment(crate::device::assigned::Error),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DeviceAssignment(error) => formatter
                .debug_tuple("DeviceAssignment")
                .field(error)
                .finish(),
            Self::RootDirectory(error) => {
                formatter.debug_tuple("RootDirectory").field(error).finish()
            }
            #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
            Self::ConsoleObject(error) => {
                formatter.debug_tuple("ConsoleObject").field(error).finish()
            }
            Self::Handle(error) => formatter.debug_tuple("Handle").field(error).finish(),
            Self::Image(error) => formatter.debug_tuple("Image").field(error).finish(),
            Self::Inspection(error) => formatter.debug_tuple("Inspection").field(error).finish(),
            Self::IncompleteThreadPublication => formatter.write_str("IncompleteThreadPublication"),
            Self::Scheduler(error) => formatter.debug_tuple("Scheduler").field(error).finish(),
            Self::MemoryObject(error) => {
                formatter.debug_tuple("MemoryObject").field(error).finish()
            }
            Self::Object(error) => formatter.debug_tuple("Object").field(error).finish(),
            Self::Process(error) => formatter.debug_tuple("Process").field(error).finish(),
            Self::Resource(error) => formatter.debug_tuple("Resource").field(error).finish(),
            Self::ResourceObject(error) => formatter
                .debug_tuple("ResourceObject")
                .field(error)
                .finish(),
            Self::Bootstrap(error) => formatter.debug_tuple("Bootstrap").field(error).finish(),
            Self::TaskGroup(error) => formatter.debug_tuple("TaskGroup").field(error).finish(),
            Self::TaskObject(error) => formatter.debug_tuple("TaskObject").field(error).finish(),
            Self::VirtualMachineObject(error) => formatter
                .debug_tuple("VirtualMachineObject")
                .field(error)
                .finish(),
        }
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<TaskGroupError> for Error {
    fn from(error: TaskGroupError) -> Self {
        Self::TaskGroup(error)
    }
}

impl From<ProcessError> for Error {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

pub(crate) fn start() -> Result<Infallible, Error> {
    // Filesystem backends may sleep. Complete process construction on a normal
    // scheduler stack after the interrupt-masked machine bootstrap has ended.
    let worker =
        scheduler::kthread_create("native-init", start_worker, 0).map_err(Error::Scheduler)?;
    if !scheduler::thread_ready(worker).map_err(Error::Scheduler)? {
        return Err(Error::IncompleteThreadPublication);
    }
    scheduler::exit_current()
}

extern "C" fn start_worker(_: usize) {
    match prepare_and_start() {
        Ok(never) => match never {},
        Err(error) => crate::boot::fail("Native init", error),
    }
}

fn prepare_and_start() -> Result<Infallible, Error> {
    let domain = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)?;
    let group = TaskGroup::try_new(&domain)?;
    let init = bootstrap::prepare(INIT_PATH, "init", &group, &domain)?;

    let channel = capabilities::install(&init, &group, &domain)?;
    let thread = init.process.create_user_thread(
        "init",
        init.process.image().initial_thread().with_argument(channel),
        scheduler::CpuMask::ALL,
    )?;

    init.process.start()?;
    let process_koid = init.process.koid().get();
    let thread_id = thread
        .scheduler_id()
        .ok_or(Error::IncompleteThreadPublication)?;
    thread.ready()?;
    crate::pr_info!(
        "HypeR: starting Native init process {} as thread {}",
        process_koid,
        thread_id.get()
    );

    // Scheduler and Process membership now own the runnable bootstrap task.
    // Keep no observer on this non-returning stack.
    drop(thread);
    drop(init);
    drop(group);
    drop(domain);
    scheduler::exit_current()
}
