// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-local stage-2 residency admission and observation.

use core::marker::PhantomData;

use hyper::cpu::PerCpu;
use hyper::sync::atomic::{AtomicU64, Ordering};

use super::Error;
use {
    crate::vm::residency_state::LocalStage2Observation,
    crate::vm::residency_state::Stage2AllocationIdentity,
    crate::vm::residency_state::Stage2Incarnation,
};

static ACTIVE_STAGE2_ROOT: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_STAGE2_EPOCH: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static VMID_EPOCH: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_STAGE2_GENERATION: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_INSTRUCTION_ROOT: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_INSTRUCTION_EPOCH: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_INSTRUCTION_TRANSLATION_EPOCH: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACTIVE_INSTRUCTION_GENERATION: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);

/// Linear residency retained from guest stage-2 activation until hardware
/// detach completes on the same CPU.
#[must_use = "an active guest residency must leave before VM execution release"]
pub(crate) struct GuestResidencyClaim {
    cpu: hyper::cpu::CpuIndex,
    admitted: Stage2Incarnation,
    _identifier: super::Stage2IdentifierLease,
    armed: bool,
    cpu_affine: PhantomData<*mut ()>,
}

impl GuestResidencyClaim {
    fn new(
        cpu: hyper::cpu::CpuIndex,
        admitted: Stage2Incarnation,
        identifier: super::Stage2IdentifierLease,
    ) -> Self {
        Self {
            cpu,
            admitted,
            _identifier: identifier,
            armed: true,
            cpu_affine: PhantomData,
        }
    }
}

impl Drop for GuestResidencyClaim {
    fn drop(&mut self) {
        if self.armed {
            // No safe destructor can prove architecture hardware is detached
            // or repair residency history after abandoning this capability.
            hyper::debug::invariant_failure("vm::memory::residency::drop invariant")
        }
    }
}

#[must_use = "failed leave retains the exact armed residency claim"]
pub(crate) struct GuestResidencyLeaveFailure {
    error: Error,
    claim: GuestResidencyClaim,
}

impl GuestResidencyLeaveFailure {
    pub(crate) const fn error(&self) -> Error {
        self.error
    }

    pub(crate) fn into_claim(self) -> GuestResidencyClaim {
        self.claim
    }
}

/// Activates the installed VM's stage-2 hierarchy on the current CPU.
///
/// # Safety
///
/// The caller must own the stopped vCPU carrying `vm`, retain this VM's
/// execution claim for this CPU, and keep local interrupts masked.
pub(crate) unsafe fn activate(
    vm: &crate::vm::registry::VmBinding,
) -> Result<GuestResidencyClaim, Error> {
    vm.with_address_space(|address_space| {
        address_space.ensure_healthy()?;
        let incarnation = address_space.incarnation()?;
        let identifier = address_space
            .active_identifier()?
            .acquire()
            .map_err(Error::Identifier)?;
        // SAFETY: The lease pins this tag and the VM lock serializes selection metadata.
        unsafe { address_space.stage2.bind_identifier(identifier.value()) }
            .map_err(Error::Stage2)?;
        let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
        if VMID_EPOCH[cpu].load(Ordering::Relaxed) != identifier.epoch() {
            crate::hal::vm::invalidate_guest_translation_namespace_local();
            VMID_EPOCH[cpu].store(identifier.epoch(), Ordering::Relaxed);
        }
        address_space
            .residency
            .check_admission(cpu.get(), incarnation.translation_epoch())
            .map_err(Error::Residency)?;
        // Residency observations remember invalidation epochs, not continued
        // hardware selection. A backend may clear its root at stopped detach.
        if !load_stage2_observation(cpu).matches(incarnation, incarnation.translation_epoch())
            || !crate::hal::vm::stage2_selection_is_current(&address_space.stage2)
        {
            // SAFETY: The caller owns the stopped vCPU, and the installed
            // address space is pinned in the VM registry for the active guest
            // lifetime. Architecture activation includes any local
            // invalidation required before this CPU may consume the current
            // mapping epoch.
            unsafe { address_space.stage2.activate() };
            store_stage2_observation(
                cpu,
                LocalStage2Observation::new(incarnation, incarnation.translation_epoch()),
            );
        }

        let instruction_epoch = address_space.instruction_epoch.load(Ordering::Acquire);
        if !load_instruction_observation(cpu).matches(incarnation, instruction_epoch) {
            // FENCE.I is hart-local on RISC-V; AArch64 and x86 likewise require
            // a local instruction synchronization event before entering a
            // newly published instruction stream on this CPU.
            crate::hal::cache::synchronize_instruction_execution();
            store_instruction_observation(
                cpu,
                LocalStage2Observation::new(incarnation, instruction_epoch),
            );
        }
        if address_space
            .residency
            .publish_admission(cpu.get())
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest residency publication failed after stage-2 activation"
            ));
        }
        Ok(GuestResidencyClaim::new(cpu, incarnation, identifier))
    })
}

/// Leaves the exact admitted CPU after architecture hardware and host timer
/// ownership have been restored but before VM execution admission is released.
pub(crate) fn leave(
    vm: &crate::vm::registry::VmBinding,
    mut claim: GuestResidencyClaim,
) -> Result<(), GuestResidencyLeaveFailure> {
    let current = match crate::cpu::current_index() {
        Some(cpu) if cpu == claim.cpu => cpu,
        _ => {
            return Err(GuestResidencyLeaveFailure {
                error: Error::InvalidCpu,
                claim,
            });
        }
    };
    let result = vm.with_address_space(|address_space| {
        let incarnation = address_space.incarnation()?;
        if !claim.admitted.same_allocation(incarnation) {
            return Err(Error::Poisoned);
        }
        address_space
            .residency
            .leave(current.get(), incarnation.translation_epoch())
            .map_err(Error::Residency)
    });
    match result {
        Ok(()) => {
            claim.armed = false;
            Ok(())
        }
        Err(error) => Err(GuestResidencyLeaveFailure { error, claim }),
    }
}

pub(super) fn publish_current_residency(incarnation: Stage2Incarnation) {
    let Some(cpu) = crate::cpu::current_index() else {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: active stage-2 mapping has no registered CPU owner"
        ));
    };
    // Active mapping publication already completed architecture-required
    // visibility and invalidation (broadcast for shared AArch64 mappings). Updating this CPU-private observation avoids repeating a
    // whole-context activation at the following IRQ-tail resume.
    store_stage2_observation(
        cpu,
        LocalStage2Observation::new(incarnation, incarnation.translation_epoch()),
    );
}

fn load_stage2_observation(cpu: hyper::cpu::CpuIndex) -> LocalStage2Observation {
    observation_from_atomics(
        ACTIVE_STAGE2_ROOT[cpu].load(Ordering::Relaxed),
        ACTIVE_STAGE2_GENERATION[cpu].load(Ordering::Relaxed),
        ACTIVE_STAGE2_EPOCH[cpu].load(Ordering::Relaxed),
        ACTIVE_STAGE2_EPOCH[cpu].load(Ordering::Relaxed),
    )
}

fn store_stage2_observation(cpu: hyper::cpu::CpuIndex, observation: LocalStage2Observation) {
    let allocation = observation.allocation();
    ACTIVE_STAGE2_ROOT[cpu].store(allocation.root(), Ordering::Relaxed);
    ACTIVE_STAGE2_GENERATION[cpu].store(allocation.generation(), Ordering::Relaxed);
    ACTIVE_STAGE2_EPOCH[cpu].store(observation.translation_epoch(), Ordering::Relaxed);
}

fn load_instruction_observation(cpu: hyper::cpu::CpuIndex) -> LocalStage2Observation {
    observation_from_atomics(
        ACTIVE_INSTRUCTION_ROOT[cpu].load(Ordering::Relaxed),
        ACTIVE_INSTRUCTION_GENERATION[cpu].load(Ordering::Relaxed),
        ACTIVE_INSTRUCTION_TRANSLATION_EPOCH[cpu].load(Ordering::Relaxed),
        ACTIVE_INSTRUCTION_EPOCH[cpu].load(Ordering::Relaxed),
    )
}

fn store_instruction_observation(cpu: hyper::cpu::CpuIndex, observation: LocalStage2Observation) {
    let allocation = observation.allocation();
    ACTIVE_INSTRUCTION_ROOT[cpu].store(allocation.root(), Ordering::Relaxed);
    ACTIVE_INSTRUCTION_GENERATION[cpu].store(allocation.generation(), Ordering::Relaxed);
    ACTIVE_INSTRUCTION_TRANSLATION_EPOCH[cpu]
        .store(observation.translation_epoch(), Ordering::Relaxed);
    ACTIVE_INSTRUCTION_EPOCH[cpu].store(observation.synchronization_epoch(), Ordering::Relaxed);
}

fn observation_from_atomics(
    root: u64,
    generation: u64,
    translation_epoch: u64,
    synchronization_epoch: u64,
) -> LocalStage2Observation {
    LocalStage2Observation::new(
        Stage2Incarnation::new(root, generation, translation_epoch),
        synchronization_epoch,
    )
}

/// Clears only per-CPU observations for the exact retained VMID allocation.
///
/// Stage-C retirement will invoke this locally after its tagged invalidation.
pub(super) fn clear_local_observations(allocation: Stage2AllocationIdentity) -> Result<(), Error> {
    let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
    let mut stage2 = load_stage2_observation(cpu);
    if stage2.clear_allocation(allocation) {
        store_stage2_observation(cpu, stage2);
    }
    let mut instruction = load_instruction_observation(cpu);
    if instruction.clear_allocation(allocation) {
        store_instruction_observation(cpu, instruction);
    }
    Ok(())
}
