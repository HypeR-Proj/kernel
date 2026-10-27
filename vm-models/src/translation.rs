// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Second-stage mapping publication lifecycle.
//!
//! Installing a leaf descriptor commits its physical-page ownership even when
//! the subsequent architectural invalidation fails. This module makes that
//! boundary explicit so VM policy cannot mistake a post-publication failure for
//! a reversible mapping failure.

use crate::exit::MemoryAccess;
use hyper::cpu::CpuIndex;
use {
    hyper::sync::atomic::AtomicBool, hyper::sync::atomic::AtomicUsize,
    hyper::sync::atomic::Ordering,
};

const INACTIVE_CPU: usize = usize::MAX;

/// Execute authority carried by one normal-memory stage-2 leaf.
///
/// Demand-zero memory starts data-only. Instruction-cache publication and the
/// transition to [`Self::ReadWriteExecute`] form one ordered transaction; this
/// keeps ordinary read/write faults off the instruction-maintenance path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage2PagePermissions {
    ReadWrite,
    ReadWriteExecute,
}

/// Architecture-neutral action for one fault against the stage-2 page model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage2FaultResolution {
    Map(Stage2PagePermissions),
    PromoteExecute,
    Refresh,
}

impl Stage2PagePermissions {
    /// Selects the least authority required to retry a decoded memory fault.
    ///
    /// A fault raised while hardware walks the guest's own page tables names a
    /// data page even when the original guest access was an instruction fetch.
    /// Executable permission is therefore granted only for a final-stage
    /// execute fault.
    pub const fn for_fault(access: MemoryAccess, during_guest_page_walk: bool) -> Self {
        if matches!(access, MemoryAccess::Execute) && !during_guest_page_walk {
            Self::ReadWriteExecute
        } else {
            Self::ReadWrite
        }
    }

    pub const fn is_executable(self) -> bool {
        matches!(self, Self::ReadWriteExecute)
    }
}

/// Resolves translation-versus-permission state without architecture policy.
pub const fn resolve_stage2_fault(
    mapped: bool,
    instruction_ready: bool,
    access: MemoryAccess,
    during_guest_page_walk: bool,
) -> Stage2FaultResolution {
    let required = Stage2PagePermissions::for_fault(access, during_guest_page_walk);
    if !mapped {
        Stage2FaultResolution::Map(required)
    } else if required.is_executable() && !instruction_ready {
        Stage2FaultResolution::PromoteExecute
    } else {
        Stage2FaultResolution::Refresh
    }
}

/// Physical-CPU history needed for guest instruction-context migration.
///
/// Host instruction publication is tracked independently by address-space
/// epochs. This state covers instructions made visible by the guest itself:
/// architectures with hart-local instruction synchronization can repair that
/// state exactly when a movable vCPU changes physical CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuestInstructionContext {
    last_cpu: Option<CpuIndex>,
}

impl GuestInstructionContext {
    pub const fn new() -> Self {
        Self { last_cpu: None }
    }

    /// Records an imminent execution interval and reports whether it migrated.
    ///
    /// The first activation is not a migration. Normal address-space
    /// instruction-publication synchronization owns its initial visibility.
    pub fn enter(&mut self, cpu: CpuIndex) -> GuestInstructionContextTransition {
        let transition = match self.last_cpu {
            None => GuestInstructionContextTransition::First,
            Some(previous) if previous == cpu => GuestInstructionContextTransition::SameCpu,
            Some(_) => GuestInstructionContextTransition::Migrated,
        };
        self.last_cpu = Some(cpu);
        transition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestInstructionContextTransition {
    First,
    SameCpu,
    Migrated,
}

/// Failure phase for an active second-stage mapping update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveMappingError<Error> {
    /// No leaf descriptor was installed; caller-owned memory remains unmapped.
    BeforeInstall(Error),
    /// The leaf descriptor is live, but architectural invalidation failed.
    ///
    /// The mapped physical memory must remain owned by the address space. The
    /// address space must not execute again until policy has completed recovery
    /// or crossed an explicit fail-stop boundary.
    InstalledButInvalidationFailed(Error),
}

/// Installs an active mapping and classifies failure around its commit point.
///
/// `install` owns descriptor construction and publication. `invalidate` runs
/// only after installation succeeds and owns the
/// architecture's local/remote translation-cache protocol.
pub fn publish_active_mapping<State, Error>(
    state: &mut State,
    install: impl FnOnce(&mut State) -> Result<(), Error>,
    invalidate: impl FnOnce(&State) -> Result<(), Error>,
) -> Result<(), ActiveMappingError<Error>> {
    install(state).map_err(ActiveMappingError::BeforeInstall)?;
    invalidate(state).map_err(ActiveMappingError::InstalledButInvalidationFailed)
}

/// Reports whether one CPU has already consumed a nonzero address-space epoch.
///
/// Epoch zero is reserved for never-observed per-CPU slots. Keeping the epoch
/// check explicit prevents physical root zero from aliasing that sentinel.
pub const fn residency_is_current(
    observed_root: u64,
    observed_epoch: u64,
    current_root: u64,
    current_epoch: u64,
) -> bool {
    current_epoch != 0 && observed_root == current_root && observed_epoch == current_epoch
}

/// Atomic single-active-vCPU contract for one pinned VM allocation.
///
/// This is a deliberately restrictive lifecycle mechanism, not a substitute
/// for a future multi-vCPU shootdown protocol. It permits a vCPU to migrate,
/// but prevents two CPUs from executing the same VM address space at once.
/// Release/acquire ordering also transfers VM execution ownership to the next
/// CPU before that CPU consumes mutable guest state.
pub struct ExclusiveExecution {
    owner: u64,
    active_cpu: AtomicUsize,
}

impl ExclusiveExecution {
    pub const fn new(owner: u64) -> Self {
        Self {
            owner,
            active_cpu: AtomicUsize::new(INACTIVE_CPU),
        }
    }

    /// Claims exclusive execution for `cpu`.
    pub fn claim(&self, cpu: CpuIndex) -> Result<ExecutionClaim, ExecutionError> {
        self.active_cpu
            .compare_exchange(
                INACTIVE_CPU,
                cpu.get(),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| ExecutionError::AlreadyActive)?;
        Ok(ExecutionClaim {
            owner: self.owner,
            cpu,
            armed: true,
        })
    }

    /// Releases the exact capability returned by [`Self::claim`] on its CPU.
    ///
    /// A wrong-CPU attempt leaves the atomic owner active. This permits a
    /// movable scheduler Thread to carry the token while preventing teardown
    /// from a CPU which does not own the live architecture context.
    pub fn release(
        &self,
        mut claim: ExecutionClaim,
        current_cpu: CpuIndex,
    ) -> Result<(), ExecutionReleaseFailure> {
        if claim.owner != self.owner {
            return Err(ExecutionReleaseFailure::new(
                ExecutionError::WrongAddressSpace,
                claim,
            ));
        }
        if claim.cpu != current_cpu {
            return Err(ExecutionReleaseFailure::new(
                ExecutionError::WrongCpu,
                claim,
            ));
        }
        if self
            .active_cpu
            .compare_exchange(
                claim.cpu.get(),
                INACTIVE_CPU,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(ExecutionReleaseFailure::new(
                ExecutionError::NotActiveOwner,
                claim,
            ));
        }
        claim.armed = false;
        Ok(())
    }
}

/// Concurrent VM execution with one exact owner per physical CPU.
/// Scheduler ownership independently excludes concurrent execution of a vCPU.
pub struct ConcurrentExecution {
    owner: u64,
    active: [AtomicBool; hyper::cpu::MAX_CPUS],
}

impl ConcurrentExecution {
    pub const fn new(owner: u64) -> Self {
        Self {
            owner,
            active: [const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS],
        }
    }

    pub fn claim(&self, cpu: CpuIndex) -> Result<ExecutionClaim, ExecutionError> {
        self.active[cpu.get()]
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| ExecutionError::AlreadyActive)?;
        Ok(ExecutionClaim {
            owner: self.owner,
            cpu,
            armed: true,
        })
    }

    pub fn release(
        &self,
        mut claim: ExecutionClaim,
        cpu: CpuIndex,
    ) -> Result<(), ExecutionReleaseFailure> {
        let error = if claim.owner != self.owner {
            Some(ExecutionError::WrongAddressSpace)
        } else if claim.cpu != cpu {
            Some(ExecutionError::WrongCpu)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(ExecutionReleaseFailure::new(error, claim));
        }
        if self.active[cpu.get()]
            .compare_exchange(true, false, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            return Err(ExecutionReleaseFailure::new(
                ExecutionError::NotActiveOwner,
                claim,
            ));
        }
        claim.armed = false;
        Ok(())
    }
}

/// Non-cloneable proof that one CPU owns a VM's execution interval.
#[derive(Debug)]
#[must_use = "an execution claim must be retained until guest execution stops"]
pub struct ExecutionClaim {
    owner: u64,
    cpu: CpuIndex,
    armed: bool,
}

impl Drop for ExecutionClaim {
    fn drop(&mut self) {
        if self.armed {
            // Losing the only release capability would make later execution
            // ownership unknowable. Report through the binary panic handler without releasing ownership.
            hyper::debug::invariant_failure("armed VM execution claim dropped")
        }
    }
}

/// Failed release which preserves the exact still-armed capability.
#[derive(Debug)]
#[must_use = "a failed release retains the execution claim"]
pub struct ExecutionReleaseFailure {
    error: ExecutionError,
    claim: ExecutionClaim,
}

impl ExecutionReleaseFailure {
    const fn new(error: ExecutionError, claim: ExecutionClaim) -> Self {
        Self { error, claim }
    }

    pub const fn error(&self) -> ExecutionError {
        self.error
    }

    pub fn into_claim(self) -> ExecutionClaim {
        self.claim
    }
}

impl ExecutionClaim {
    pub const fn cpu(&self) -> CpuIndex {
        self.cpu
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionError {
    AlreadyActive,
    NotActiveOwner,
    WrongAddressSpace,
    WrongCpu,
}
