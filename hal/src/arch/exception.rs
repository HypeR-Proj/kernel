// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture exception and fail-stop mechanisms.
//!
//! Kernel crash and IRQ policy owns the decision to stop or resume. This
//! facade exposes only machine context capture, vector installation, emergency
//! stack entry, and crash-stop delivery. Raw exception entry remains in the
//! selected backend and reaches policy through immutable registered services.

use core::fmt;

use hyper::cpu::CpuIndex;
use hyper::sync::PublishedOnce;

pub use super::imp::{CrashContext, RuntimeVectorError};

/// A stack frame failed alignment or retained-stack bounds validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidStackFrame;

type FatalCallback = for<'reason> fn(CrashContext, fmt::Arguments<'reason>) -> !;

static FATAL_SERVICE: PublishedOnce<FatalCallback> = PublishedOnce::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryServiceError {
    AlreadyInstalled,
}

#[derive(Clone, Copy)]
pub(crate) struct FatalEntryReady {
    _private: (),
}

pub(crate) fn install_fatal_service(
    callback: FatalCallback,
) -> Result<FatalEntryReady, EntryServiceError> {
    FATAL_SERVICE
        .publish(callback)
        .map_err(|_| EntryServiceError::AlreadyInstalled)?;
    Ok(FatalEntryReady { _private: () })
}

pub(crate) fn fatal(context: CrashContext, reason: fmt::Arguments<'_>) -> ! {
    let Some(callback) = FATAL_SERVICE.get().copied() else {
        super::imp::halt()
    };
    callback(context, reason)
}

pub(crate) use super::imp::{
    bootstrap_stack_bounds, broadcast_crash_stop, capture_crash_context, crash_stop_interrupt,
    is_crash_stop_interrupt, validate_local_runtime_vectors, validate_runtime_vectors,
};

/// Publishes the pinned exception stacks belonging to one logical CPU.
///
/// # Safety
///
/// Both ranges must be nonempty, ABI-aligned, exclusively reserved stack
/// mappings which remain pinned and writable for the CPU's online lifetime.
/// The target CPU must not enter runtime exceptions until publication
/// completes, and its slot may be installed only once.
#[inline]
pub(crate) unsafe fn install_exception_stacks(
    cpu: CpuIndex,
    irq: (usize, usize),
    emergency: (usize, usize),
) -> Result<(), ()> {
    // SAFETY: The facade preserves both backend range/lifetime contracts and
    // passes an index already validated against the common CPU-slot capacity.
    unsafe { super::imp::install_exception_stacks(cpu.get(), irq, emergency) }
}

/// Installs the selected architecture's permanent runtime exception vectors.
///
/// # Safety
///
/// The final executable kernel mapping and a valid exception stack must be
/// active. Kernel entry services must be initialized and local interrupts must
/// remain masked until installation and validation complete. Any backend
/// one-time installation requirement must also be upheld.
#[inline]
pub(crate) unsafe fn install_runtime_vectors() {
    // SAFETY: This facade preserves the selected backend's mapping, entry
    // service, interrupt-state, and one-time publication requirements.
    unsafe { super::imp::install_runtime_vectors() }
}

/// Installs already-published runtime vector state on the calling CPU.
///
/// # Safety
///
/// The global vector representation must be immutable and published. This CPU
/// must own installed exception stacks and keep local interrupts masked until
/// its local vector state has been validated.
#[inline]
pub(crate) unsafe fn install_local_runtime_vectors() {
    // SAFETY: The facade preserves the selected backend's per-CPU stack,
    // mapping, publication, and interrupt-mask requirements.
    unsafe { super::imp::install_local_runtime_vectors() }
}

/// Permanently invokes fatal handling on the current CPU's emergency stack.
///
/// # Safety
///
/// The current CPU's emergency stack must be installed, pinned, and not
/// already active. Local interrupts must be masked. `argument` and any storage
/// it identifies must remain mapped and unreused until `callback` stops the
/// CPU. The callback must not publish a borrow of abandoned-stack storage or
/// allow one to escape the permanent fail-stop continuation.
#[inline]
pub(crate) unsafe fn run_on_emergency_stack(
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The facade forwards the strict common emergency-stack lifetime,
    // exclusivity, interrupt-state, and callback argument contract.
    unsafe { super::imp::run_on_emergency_stack(callback, argument) }
}
