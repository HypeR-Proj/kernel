// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-object identity, concrete payloads, and level-state observation.
//!
//! Capability handles grant authority to reach these objects. This subsystem
//! owns payload policy after a handle has been resolved and the Process handle
//! lock has been released.

use crate::authority::Rights;

mod core;
pub(crate) mod diagnostics;
mod directory;
mod event;
mod identity;
mod signals;
mod wait;
mod wait_set;
pub(crate) use wait_set::{WaitSet, WaitSetError};

#[cfg(test)]
pub(crate) use core::reap_final_objects;
pub(crate) use core::{
    ActiveHandleError, ActiveHandleOwner, Diagnostic, ErasedKernelRef, KernelObject, KernelRef,
    KernelService, Koid, ObjectCreationError, ObjectHandleState, ObjectKind, ObjectPublication,
    ObjectRetirement, ObjectSnapshot, OperationPin, PublishableRef, Scheduler, TransferClass,
    UserExportableObject, VmDeviceBinding, final_reap_pending, object_allocation_size, private,
    reap_one_final_object,
};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use directory::retain_for_test;
pub(crate) use directory::{ObjectScanCursor, lookup_diagnostic, scan};
pub(crate) use event::{Event, EventError};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use signals::PreparedSignalWait;
pub(crate) use signals::{
    SignalMask, SignalSource, SignalState, SignalWaitError, SignalWaitOutcome,
};
pub(crate) use wait::{
    ObjectWaitError, PreparedTimedWait, PublishedTimedWait, SignalWaitManyOutcome,
    SignalWaitRequest, TimedWaitPreparation, prepare_timed_wait, wait_many, wait_one,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use wait_set::test_delivery_rollback;
