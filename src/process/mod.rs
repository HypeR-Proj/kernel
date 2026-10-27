// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native Process, `TaskGroup`, and `UserThread` lifecycle ownership.

pub(crate) mod atomic_wait;
pub(crate) mod bootstrap;
pub(crate) mod formats;

mod builder;
mod builder_input;
mod builder_policy;
mod directory;
pub(crate) mod hierarchy;
mod image;
mod invocation;
mod lifecycle;
pub(crate) mod loader;
mod objects;
mod owner;
pub(crate) mod retention;
pub(crate) mod scheduling;
mod task_group;
mod user_thread;

pub(crate) use builder::{
    ProcessBuilder, ProcessBuilderError, StartupCapability, abort_process_builder,
    create_process_builder, start_process_builder,
};
pub(crate) use directory::{
    FilePageReclaimCursor, ProcessDiagnosticRef, ProcessScanCursor, reclaim_file_pages, scan,
};
pub(crate) use image::{ImageError, MachineAbi, ProcessImage, UserThreadStart};
pub(crate) use lifecycle::{ProcessPhase, TerminalReason, UserThreadPhase};
pub(crate) use loader::{Error as LoaderError, load_native};
pub(crate) use objects::{ProcessObject, TaskFactory, TaskGroupObject, TaskObjectError};
pub(crate) use owner::{
    ChildProcessStartError, PreparedDirectProcessHandleTransfer, PreparedProcess, Process,
    ProcessError, ProcessHandleBatchReservation, ProcessId, ProcessSnapshot, ProcessStopReport,
    promote_delayed_retirements, reap_one_process, retirement_work,
};
pub(crate) use task_group::{TaskGroup, TaskGroupError, TaskGroupId};
pub(crate) use user_thread::UserExecutionOwnership;
pub(crate) use user_thread::{RunAdmissionError, UserExecution, UserThread};

#[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
pub(crate) use owner::ProcessHandleReservation;

#[cfg_attr(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    allow(
        unused_imports,
        reason = "Native diagnostics tests require user execution support"
    )
)]
pub(crate) use owner::ProcessNameSnapshot;
