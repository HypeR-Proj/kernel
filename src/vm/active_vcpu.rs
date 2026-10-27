// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-CPU publication of the pinned vCPU currently executing at lower EL.
//!
//! These slots are exclusively local callback ownership. Remote interrupt
//! injection and vCPU kicks require a separate durable running-CPU publication
//! and must never inspect or complete this raw-pointer state.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::align_of;
use core::ptr::NonNull;

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{Ordering, compiler_fence};
use hyper::sync::{AtomicBorrowClaim, AtomicBorrowError, AtomicBorrowPtr};

use crate::vm::vcpu::VcpuExecution;

static ACTIVE: PerCpu<AtomicBorrowPtr<VcpuExecution>> =
    PerCpu::new([const { AtomicBorrowPtr::new() }; hyper::cpu::MAX_CPUS]);
static OWNERSHIP: PerCpu<ActiveOwnershipSlot> =
    PerCpu::new([const { ActiveOwnershipSlot::new() }; hyper::cpu::MAX_CPUS]);

struct ActiveOwnership {
    execution: NonNull<VcpuExecution>,
    claim: Option<super::registry::VmExecutionClaim>,
}

struct ActiveOwnershipSlot {
    local: UnsafeCell<Option<ActiveOwnership>>,
}

impl ActiveOwnershipSlot {
    const fn new() -> Self {
        Self {
            local: UnsafeCell::new(None),
        }
    }

    /// Installs ownership before callback publication on the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must be executing on this slot's CPU with local IRQs masked.
    unsafe fn install(&self, ownership: ActiveOwnership) -> Result<(), ActiveOwnership> {
        // SAFETY: CPU locality plus the IRQ mask excludes every other access.
        let local = unsafe { &mut *self.local.get() };
        if local.is_some() {
            return Err(ownership);
        }
        *local = Some(ownership);
        Ok(())
    }

    /// Removes the exact ownership after callback publication is gone.
    ///
    /// # Safety
    ///
    /// The caller must be executing on this slot's CPU with local IRQs masked.
    unsafe fn take(&self, execution: NonNull<VcpuExecution>) -> Option<ActiveOwnership> {
        // SAFETY: CPU locality plus the IRQ mask excludes every other access.
        let local = unsafe { &mut *self.local.get() };
        match local.as_ref() {
            Some(owner) if owner.execution == execution => local.take(),
            _ => None,
        }
    }
}

// SAFETY: a slot is accessed only by its owning CPU with local IRQs masked.
// The contained claim never crosses CPUs; `PerCpu` indexing is validated from
// the registered current CPU before either unsafe slot operation.
unsafe impl Sync for ActiveOwnershipSlot {}

const _: () = assert!(align_of::<VcpuExecution>() >= 2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ActiveVcpuMissing,
    CpuAlreadyActive,
    InterruptsEnabled,
    InvalidCpu,
    InvalidExecution,
    Preemption(crate::task::scheduler::Error),
    ReentrantAccess,
}

/// Publishes the pinned execution owned by the local guest run loop.
///
/// # Safety
///
/// `execution` must be the scheduler-origin raw pointer, not a pointer derived
/// from a temporary Rust reference. It must remain at this address and
/// exclusively associated with the calling CPU until [`clear`] succeeds.
/// Local IRQs must be masked across publication and guest entry.
pub unsafe fn set_raw(
    execution: *mut VcpuExecution,
    claim: Option<super::registry::VmExecutionClaim>,
) -> Result<(), PublicationFailure> {
    if execution.is_null() || !execution.is_aligned() {
        return Err(PublicationFailure::new(Error::InvalidExecution, claim));
    }
    if let Err(error) = ensure_interrupts_masked() {
        return Err(PublicationFailure::new(error, claim));
    }
    let cpu = match current_cpu() {
        Ok(cpu) => cpu,
        Err(error) => return Err(PublicationFailure::new(error, claim)),
    };
    let Some(execution) = NonNull::new(execution) else {
        return Err(PublicationFailure::new(Error::InvalidExecution, claim));
    };
    // SAFETY: current CPU validation and the local IRQ mask establish the slot contract.
    if let Err(owner) = unsafe { OWNERSHIP[cpu].install(ActiveOwnership { execution, claim }) } {
        return Err(PublicationFailure::new(
            Error::CpuAlreadyActive,
            owner.claim,
        ));
    }
    // Complete hardware activation accesses before a callback can observe the
    // raw owner token. This is a same-CPU compiler ownership boundary.
    compiler_fence(Ordering::Release);
    if let Err(error) = ACTIVE[cpu].publish(execution) {
        // SAFETY: publication failed and the same local masked CPU still owns the slot.
        let owner = unsafe { OWNERSHIP[cpu].take(execution) }.unwrap_or_else(|| {
            hyper::debug::invariant_failure("vm::active_vcpu::set_raw invariant")
        });
        return Err(PublicationFailure::new(
            match error {
                AtomicBorrowError::Active => Error::CpuAlreadyActive,
                AtomicBorrowError::InvalidPointer => Error::InvalidExecution,
                AtomicBorrowError::Borrowed
                | AtomicBorrowError::Inactive
                | AtomicBorrowError::NotBorrowed
                | AtomicBorrowError::PointerMismatch => Error::CpuAlreadyActive,
            },
            owner.claim,
        ));
    }
    Ok(())
}

pub fn clear(
    execution: &mut VcpuExecution,
) -> Result<Option<super::registry::VmExecutionClaim>, Error> {
    ensure_interrupts_masked()?;
    let cpu = current_cpu()?;
    ACTIVE[cpu]
        .unpublish(NonNull::from(&mut *execution))
        .map_err(|error| match error {
            AtomicBorrowError::Borrowed => Error::ReentrantAccess,
            AtomicBorrowError::InvalidPointer => Error::InvalidExecution,
            AtomicBorrowError::Active
            | AtomicBorrowError::Inactive
            | AtomicBorrowError::NotBorrowed
            | AtomicBorrowError::PointerMismatch => Error::ActiveVcpuMissing,
        })?;
    // Do not let subsequent hardware detachment move above removal of callback
    // visibility. No inter-CPU memory publication is implied or required.
    compiler_fence(Ordering::Acquire);
    let pointer = NonNull::from(&mut *execution);
    // SAFETY: unpublication succeeded on this same IRQ-masked CPU.
    let owner = unsafe { OWNERSHIP[cpu].take(pointer) }
        .unwrap_or_else(|| hyper::debug::invariant_failure("vm::active_vcpu::clear invariant"));
    Ok(owner.claim)
}

pub struct PublicationFailure {
    error: Error,
    claim: Option<super::registry::VmExecutionClaim>,
}

impl PublicationFailure {
    const fn new(error: Error, claim: Option<super::registry::VmExecutionClaim>) -> Self {
        Self { error, claim }
    }

    pub const fn error(&self) -> Error {
        self.error
    }

    pub fn into_claim(mut self) -> Option<super::registry::VmExecutionClaim> {
        self.claim.take()
    }
}

impl Drop for PublicationFailure {
    fn drop(&mut self) {
        if self.claim.is_some() {
            // Dropping a failed publication would abandon exclusive VM and
            // CPU-residency ownership without architecture teardown.
            hyper::debug::invariant_failure("vm::active_vcpu::drop invariant")
        }
    }
}

pub fn with<R>(operation: impl FnOnce(&mut VcpuExecution) -> R) -> Result<Option<R>, Error> {
    with_pinned(|execution, _pin| operation(execution))
}

/// Borrows the active vCPU together with proof that the callback cannot
/// migrate between CPUs.
///
/// Most VM-exit handlers should use [`with`]. This variant exists for bounded
/// architecture protocols, such as multi-pass cache maintenance, whose HAL
/// contract requires an explicit CPU-affinity witness.
pub(crate) fn with_pinned<R>(
    operation: impl FnOnce(&mut VcpuExecution, &dyn hyper::cpu::PinnedExecution) -> R,
) -> Result<Option<R>, Error> {
    ensure_interrupts_masked()?;
    // A local IRQ mask prevents asynchronous scheduler entry, while this guard
    // also excludes an explicit scheduling point introduced by a future
    // callback. The active slot therefore remains owned by one CPU for the
    // complete raw-pointer borrow.
    let preemption = crate::task::scheduler::preempt_disable().map_err(Error::Preemption)?;
    let cpu = current_cpu()?;
    let claim = ACTIVE[cpu].begin_borrow().map_err(|error| match error {
        AtomicBorrowError::Borrowed => Error::ReentrantAccess,
        AtomicBorrowError::InvalidPointer => Error::InvalidExecution,
        AtomicBorrowError::Active
        | AtomicBorrowError::Inactive
        | AtomicBorrowError::NotBorrowed
        | AtomicBorrowError::PointerMismatch => Error::ActiveVcpuMissing,
    })?;
    let Some(claim) = claim else {
        return Ok(None);
    };
    let execution = claim.pointer();
    let borrow = ActiveBorrow {
        cpu,
        claim: Some(claim),
        cpu_affine: PhantomData,
    };
    // This is a compiler ownership boundary, not cross-CPU publication. The
    // tagged atomic state makes exception re-entry observe Borrowed, while the
    // preemption guard pins the continuation. No hardware barrier is needed.
    compiler_fence(Ordering::Acquire);
    // SAFETY: publication requires the execution to remain pinned and
    // exclusively associated with this CPU. Exception entry keeps local IRQs
    // masked and scopes the reconstructed reference to this callback rather
    // than a caller-selected lifetime.
    let result = unsafe { operation(&mut *execution.as_ptr(), &preemption) };
    borrow.complete();
    drop(preemption);
    Ok(Some(result))
}

struct ActiveBorrow {
    cpu: CpuIndex,
    claim: Option<AtomicBorrowClaim<'static, VcpuExecution>>,
    // The guard owns one CPU-local state transition and must never cross a
    // scheduler or exception continuation.
    cpu_affine: PhantomData<*mut ()>,
}

impl ActiveBorrow {
    fn complete(mut self) {
        if let Err(error) = self.complete_inner() {
            fail_borrow_completion(error)
        }
    }

    fn complete_inner(&mut self) -> Result<(), Error> {
        if self.claim.is_none() {
            return Ok(());
        }
        if current_cpu()? != self.cpu {
            return Err(Error::InvalidCpu);
        }
        ensure_interrupts_masked()?;
        // End every access through the reconstructed exclusive reference
        // before the state can become borrowable again. CPU pinning and the
        // atomic tag provide execution ownership; this fence is compiler-only.
        compiler_fence(Ordering::Release);
        let claim = self.claim.take().ok_or(Error::ActiveVcpuMissing)?;
        claim.finish().map_err(|_| Error::ReentrantAccess)
    }
}

impl Drop for ActiveBorrow {
    fn drop(&mut self) {
        if let Err(error) = self.complete_inner() {
            fail_borrow_completion(error)
        }
    }
}

fn fail_borrow_completion(error: Error) -> ! {
    // Continuing could expose a second mutable reference to the pinned vCPU.
    // Halt without unwinding so both the Borrowed state and its preemption pin
    // remain retained. This path may inherit arbitrary callback lock state, so
    // only the allocation-free, ordinary-lock-free panic path is safe.
    let _ = error;
    hyper::debug::invariant_failure("vm::active_vcpu::fail_borrow_completion invariant")
}

fn ensure_interrupts_masked() -> Result<(), Error> {
    if crate::hal::irq::local_enabled() {
        Err(Error::InterruptsEnabled)
    } else {
        Ok(())
    }
}

fn current_cpu() -> Result<CpuIndex, Error> {
    crate::cpu::current_index().ok_or(Error::InvalidCpu)
}
