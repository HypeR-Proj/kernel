// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability-oriented service boundaries consumed by Native ABI handlers.
//!
//! Each trait describes one cohesive kernel domain. The dispatch layer uses
//! composite marker traits only to state that one concrete session supplies
//! the complete route set; no operation silently falls back to a production
//! `not supported` implementation.

use hyper::abi::native::NativeResult;

mod console;
mod device;
mod filesystem;
mod inspect;
mod ipc;
mod memory;
mod object;
mod process;
mod synchronization;
mod vm;

pub(crate) use console::{ConsoleServiceError, ConsoleServices};
pub(crate) use device::DeviceServices;
pub(crate) use filesystem::VfsServices;
pub(crate) use inspect::{InspectServices, SystemInspectServices};
pub(crate) use ipc::IpcServices;
pub(crate) use memory::{MemoryServices, UserMemoryServices};
pub(crate) use object::{HandleServices, ObjectServiceError, ObjectServices};
pub(crate) use process::{
    HierarchyServices, ProcessBuilderServiceError, ProcessBuilderServices, RetentionServices,
    TaskServices,
};
pub(crate) use synchronization::{SyncWaitRequest, SynchronizationServices};
pub(crate) use vm::{GuestIoServices, VmServices};

pub(crate) trait NativeServices:
    UserMemoryServices
    + HandleServices
    + ObjectServices
    + IpcServices
    + HierarchyServices
    + VmServices
    + DeviceServices
    + GuestIoServices
    + SystemInspectServices
    + InspectServices
    + ConsoleServices
    + VfsServices
    + MemoryServices
    + SynchronizationServices
    + ProcessBuilderServices
    + RetentionServices
    + TaskServices
{
}

impl<T> NativeServices for T where
    T: UserMemoryServices
        + HandleServices
        + ObjectServices
        + IpcServices
        + HierarchyServices
        + VmServices
        + DeviceServices
        + GuestIoServices
        + SystemInspectServices
        + InspectServices
        + ConsoleServices
        + VfsServices
        + MemoryServices
        + SynchronizationServices
        + ProcessBuilderServices
        + RetentionServices
        + TaskServices
{
}

/// Policy action produced by an ordinary Native syscall.
///
/// This value owns no architecture frame or CPU-affine state. The Thread
/// continuation applies the scheduling or lifecycle effect before user return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeAction {
    Return(NativeResult),
    Yield(NativeResult),
    ExitThread { status: i64 },
    ExitProcess { status: i64 },
}
