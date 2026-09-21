// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected exception-entry and fail-stop machine capabilities.
//!
//! Kernel crash and IRQ services own failure policy. This facade selects only
//! context capture, vector and stack installation, emergency-stack entry, and
//! crash-stop transport. Raw architecture entry invokes kernel policy only
//! through immutable services published before runtime vectors are installed.

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::InterruptId;

/// Selected raw diagnostic context and vector-validation error.
///
/// The context deliberately retains each architecture's register model. Crash
/// reporting is compiled for one backend and must not erase useful machine
/// state behind a false lowest-common-denominator representation.
pub use crate::arch::exception::{CrashContext, InvalidStackFrame, RuntimeVectorError};

/// Proof that execution is inside the selected architecture's qualified IRQ
/// return continuation.
///
/// This type has no public constructor. Only backends with a private-stack
/// return continuation can mint it; unsupported backends therefore cannot
/// accidentally enter the IRQ-tail scheduling path.
pub struct IrqTailCapability {
    // The proof describes the current CPU's masked interrupt continuation and
    // must not become transferable even if a later caller adds scoped work.
    not_send_or_sync: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

impl IrqTailCapability {
    /// Ends the CPU-affine proof before a scheduler transition may migrate the
    /// resumed thread continuation.
    pub fn consume(self) {}
}

/// Executes kernel tail policy while the selected architecture's qualified
/// interrupt-return contract is active.
///
/// # Safety
///
/// The caller must be a selected post-acknowledgement continuation running on
/// the interrupted Thread stack with local IRQs masked and without any raw
/// exception-frame borrow crossing `service`.
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub unsafe fn with_irq_tail_capability(service: fn(IrqTailCapability)) {
    service(IrqTailCapability {
        not_send_or_sync: core::marker::PhantomData,
    });
}

/// Rejects IRQ-tail scheduling on a backend without a qualified continuation.
///
/// # Safety
///
/// This backend provides no valid invocation context. Callers must first use
/// `qualify_irq_tail_postlude` and invoke a postlude only when it is admitted.
#[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
pub unsafe fn with_irq_tail_capability(_service: fn(IrqTailCapability)) {
    // This function is referenced by the stable kernel postlude, but the
    // selected backend below never publishes that postlude on this target.
    // Fail closed if an invalid caller bypasses the selection contract.
    hyper::debug::invariant_failure("selected HAL exception::with_irq_tail_capability invariant")
}

/// Qualifies a kernel IRQ-tail postlude for the selected entry backend.
///
/// `None` means interrupt return cannot safely switch a Thread context. The
/// scheduler retains any pending request for a later cooperative safe point.
pub const fn qualify_irq_tail_postlude(
    origin: hyper::hal::interrupt::InterruptOrigin,
    postlude: unsafe extern "C" fn(),
) -> Option<unsafe extern "C" fn()> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        match origin {
            hyper::hal::interrupt::InterruptOrigin::Host
            | hyper::hal::interrupt::InterruptOrigin::Guest => Some(postlude),
            hyper::hal::interrupt::InterruptOrigin::Native { .. } => None,
        }
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        match origin {
            hyper::hal::interrupt::InterruptOrigin::Guest => Some(postlude),
            hyper::hal::interrupt::InterruptOrigin::Host
            | hyper::hal::interrupt::InterruptOrigin::Native { .. } => None,
        }
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    {
        let _ = (origin, postlude);
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryServiceError {
    Fatal(crate::arch::exception::EntryServiceError),
    Interrupt(crate::arch::irq::InterruptEntryServiceError),
}

/// Proof that fatal and physical-interrupt policy callbacks are immutable.
#[derive(Clone, Copy)]
pub struct EntryReady {
    fatal: crate::arch::exception::FatalEntryReady,
    interrupt: crate::arch::irq::InterruptEntryReady,
}

pub fn install_entry_services(
    fatal: for<'reason> fn(CrashContext, core::fmt::Arguments<'reason>) -> !,
    dispatch: fn(
        InterruptId,
        hyper::hal::interrupt::InterruptOrigin,
    ) -> hyper::hal::interrupt::EntryAction,
    claim_external: fn(
        hyper::hal::interrupt::InterruptOrigin,
    ) -> Option<hyper::hal::interrupt::EntryAction>,
    stop: fn(CrashContext) -> !,
) -> Result<EntryReady, EntryServiceError> {
    let fatal =
        crate::arch::exception::install_fatal_service(fatal).map_err(EntryServiceError::Fatal)?;
    let interrupt = {
        #[cfg(CONFIG_ARCH_RISCV64)]
        {
            crate::arch::irq::install_interrupt_entry_services(
                crate::arch::irq::InterruptEntryServices::new(dispatch, claim_external, stop),
            )
        }
        #[cfg(not(CONFIG_ARCH_RISCV64))]
        {
            let _ = claim_external;
            crate::arch::irq::install_interrupt_entry_services(
                crate::arch::irq::InterruptEntryServices::new(dispatch, stop),
            )
        }
    }
    .map_err(EntryServiceError::Interrupt)?;
    Ok(EntryReady { fatal, interrupt })
}

#[inline]
pub fn capture_crash_context() -> CrashContext {
    crate::arch::exception::capture_crash_context()
}

#[inline]
pub fn bootstrap_stack_bounds(stack_pointer: u64) -> Option<(usize, usize)> {
    crate::arch::exception::bootstrap_stack_bounds(stack_pointer)
}

#[inline]
pub fn crash_stop_interrupt() -> Option<InterruptId> {
    crate::arch::exception::crash_stop_interrupt()
}

#[inline]
pub fn is_crash_stop_interrupt(interrupt: InterruptId) -> bool {
    crate::arch::exception::is_crash_stop_interrupt(interrupt)
}

#[inline]
pub fn broadcast_crash_stop() -> bool {
    crate::arch::exception::broadcast_crash_stop()
}

/// Exception stack ranges or their CPU slot could not be registered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExceptionStackError;

/// Publishes the pinned exception stacks belonging to one logical CPU.
///
/// # Safety
///
/// Both ranges must be nonempty, ABI-aligned, exclusively reserved stack
/// mappings which remain pinned and writable for the CPU's online lifetime.
/// The target CPU must not enter runtime exceptions until publication
/// completes, and its slot may be installed only once.
#[inline]
pub unsafe fn install_exception_stacks(
    cpu: CpuIndex,
    irq: (usize, usize),
    emergency: (usize, usize),
) -> Result<(), ExceptionStackError> {
    // SAFETY: The selected architecture facade receives the same validated
    // CPU index, exclusive ranges, lifetime, and publication guarantees.
    unsafe { crate::arch::exception::install_exception_stacks(cpu, irq, emergency) }
        .map_err(|()| ExceptionStackError)
}

/// Installs the selected architecture's permanent runtime exception vectors.
///
/// # Safety
///
/// The final executable kernel mapping and a valid exception stack must be
/// active. Kernel entry services must be initialized and local interrupts must
/// remain masked until installation and validation complete. The selected
/// backend's one-time installation requirement must also be upheld.
#[inline]
pub unsafe fn install_runtime_vectors(entry: &EntryReady) {
    let _ = (&entry.fatal, &entry.interrupt);
    // SAFETY: The selected architecture facade receives the same mapping,
    // entry-service, mask-state, and one-time publication guarantees.
    unsafe { crate::arch::exception::install_runtime_vectors() }
}

/// Installs the selected runtime vector state on the calling secondary CPU.
///
/// # Safety
///
/// The global vector representation must already be immutable and published.
/// The calling CPU must own installed exception stacks and keep local
/// interrupts masked through validation.
#[inline]
pub unsafe fn install_local_runtime_vectors(entry: &EntryReady) {
    let _ = (&entry.fatal, &entry.interrupt);
    // SAFETY: The selected backend receives the same per-CPU stack, mapping,
    // publication, and interrupt-mask guarantees.
    unsafe { crate::arch::exception::install_local_runtime_vectors() }
}

#[inline]
pub fn validate_runtime_vectors() -> Result<(), RuntimeVectorError> {
    crate::arch::exception::validate_runtime_vectors()
}

#[inline]
pub fn validate_local_runtime_vectors() -> Result<(), RuntimeVectorError> {
    crate::arch::exception::validate_local_runtime_vectors()
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
pub unsafe fn run_on_emergency_stack(callback: extern "C" fn(usize) -> !, argument: usize) -> ! {
    // SAFETY: The selected architecture facade receives the same stack
    // lifetime, exclusivity, mask-state, and callback argument guarantees.
    unsafe { crate::arch::exception::run_on_emergency_stack(callback, argument) }
}
