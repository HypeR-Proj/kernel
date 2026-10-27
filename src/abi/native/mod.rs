// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `HypeR` Native syscall validation and dispatch.

/// Register arguments owned by one decoded Native syscall invocation.
pub(super) type Arguments = [u64; hyper::abi::native::HYPER_NATIVE_INVOCATION_ARGUMENT_REGISTERS];

mod services;

pub(crate) use services::{
    ConsoleServiceError, ConsoleServices, DeviceServices, GuestIoServices, HandleServices,
    HierarchyServices, InspectServices, IpcServices, MemoryServices, NativeAction,
    ObjectServiceError, ObjectServices, ProcessBuilderServiceError, ProcessBuilderServices,
    RetentionServices, SystemInspectServices, TaskServices, UserMemoryServices, VfsServices,
    VmServices,
};

mod dispatch;

pub(crate) use dispatch::dispatch;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod admission_test;
mod handlers;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
mod self_test;
mod status;
mod wire;

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use self_test::{SelfTestError, run_self_test};
