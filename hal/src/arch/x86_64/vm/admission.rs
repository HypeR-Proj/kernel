// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! x86 hardware-virtualization admission checks.
//!
//! VMX and SVM decode owned guest-exit events in their respective backends;
//! no common raw synchronous frame exists at this architecture boundary.

use hyper_vm_models::x86::merge_port_input;
use {
    hyper_vm_models::x86::exit::PortIoAction, hyper_vm_models::x86::exit::PortIoExit,
    hyper_vm_models::x86::exit::PortIoOperation,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    HardwareUnavailable,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    SecondLevelPagingUnavailable,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    MissingNextRip,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    BackendConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PortIoCompletion {
    Input(u64),
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PortIoCompletionError {
    ActionMismatch,
    InvalidWidth,
    PolicyStopped,
}

/// Validates a policy action against its originating port operation.
///
/// Both x86 backends share accumulator semantics but retain distinct machine
/// completion. Keeping this step pure ensures an input result cannot be
/// applied to an output exit, or vice versa.
pub(crate) fn complete_port_io(
    accumulator: u64,
    exit: PortIoExit,
    action: PortIoAction,
) -> Result<PortIoCompletion, PortIoCompletionError> {
    match (exit.operation(), action) {
        (PortIoOperation::Input, PortIoAction::CompleteInput(value)) => {
            merge_port_input(accumulator, value, exit.width().bytes())
                .map(PortIoCompletion::Input)
                .ok_or(PortIoCompletionError::InvalidWidth)
        }
        (PortIoOperation::Output(_), PortIoAction::CompleteOutput) => Ok(PortIoCompletion::Output),
        (_, PortIoAction::Stop) => Err(PortIoCompletionError::PolicyStopped),
        _ => Err(PortIoCompletionError::ActionMismatch),
    }
}

pub(crate) fn validate() -> Result<(), ValidationError> {
    // Backend selection belongs to Linux guest admission, not host exception-
    // vector initialization.
    Ok(())
}
