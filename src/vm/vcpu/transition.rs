// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! vCPU hardware activation, reconciliation, and exact teardown transactions.

/// Claims VM execution, selects the current mapping epoch, activates local
/// hardware, then publishes the pinned execution for exception callbacks.
/// Publication is last so callbacks cannot observe partial state.
///
/// # Safety
///
/// `execution` must be the scheduler-origin, non-null, aligned, pinned, and
/// exclusively owned current-vCPU pointer. Local interrupts must be masked.
pub(crate) unsafe fn activate(
    execution: *mut super::VcpuExecution,
) -> Result<(), HardwareTransitionError> {
    if execution.is_null() || !execution.is_aligned() {
        return Err(HardwareTransitionError::InvalidExecution);
    }
    let mut execution_claim;
    let consumed_reconcile;
    {
        // SAFETY: The caller supplies the valid, pinned, exclusive pointer.
        let execution = unsafe { &mut *execution };
        let cpu = crate::cpu::current_index().ok_or(HardwareTransitionError::InvalidExecution)?;
        execution_claim = claim_execution(execution, cpu)?;
        if execution.enter_instruction_context(cpu)
            == hyper_vm_models::translation::GuestInstructionContextTransition::Migrated
        {
            crate::hal::cache::synchronize_guest_instruction_migration();
        }
        if let Some(claim) = execution_claim.as_mut() {
            let Some(binding) = execution.vm_binding() else {
                release_execution_or_fail(execution, execution_claim);
                return Err(HardwareTransitionError::InvalidExecution);
            };
            // SAFETY: The execution claim exclusively owns this physical CPU
            // for this VM. Shared address-space serialization protects epoch
            // selection while other vCPUs execute. This vCPU is stopped and
            // local IRQs are masked as required by the caller.
            let residency = match unsafe { super::memory::activate(binding) } {
                Ok(residency) => residency,
                Err(error) => {
                    release_execution_or_fail(execution, execution_claim);
                    return Err(HardwareTransitionError::Memory(error));
                }
            };
            // The residency capability becomes part of the exact composite VM
            // execution claim before any later fallible activation step.
            if let Err(_residency) = claim.attach_residency(residency) {
                hyper::debug::invariant_failure("vm::vcpu::transition::activate invariant")
            }
        }
        // Clear publication before acquiring the controller lock. A producer
        // which mutates after this exchange publishes a new bit that survives
        // this activation and prompts the scheduler-authoritative running CPU.
        let reconcile_claimed = match execution.vm_binding() {
            Some(binding) => match binding.claim_detached_interrupt_reconcile(execution.vcpu_id) {
                Ok(claimed) => claimed,
                Err(error) => {
                    release_execution_or_fail(execution, execution_claim);
                    return Err(HardwareTransitionError::Registry(error));
                }
            },
            None => false,
        };
        consumed_reconcile = reconcile_claimed;
        let interrupts = core::ptr::from_ref(execution.interrupts());
        // SAFETY: The VM binding keeps the controller fixed and live; this
        // reference ends before raw active-vCPU publication.
        let interrupts = unsafe { &*interrupts };
        // SAFETY: The caller owns this stopped vCPU with IRQs masked.
        let timer_asserted = match unsafe {
            crate::hal::vm::activate_hardware(
                &mut execution.hardware,
                execution.vcpu_id,
                interrupts,
                crate::time::monotonic_ticks(),
            )
        } {
            Ok(asserted) => asserted,
            Err(error) => {
                restore_reconcile_if_claimed(execution, reconcile_claimed);
                release_execution_or_fail(execution, execution_claim);
                return Err(if crate::hal::vm::interrupt_entry_deferred(&error) {
                    HardwareTransitionError::InterruptGateClosed
                } else {
                    HardwareTransitionError::Hardware(error)
                });
            }
        };
        if let Err(timer) = super::timer::activate_guest_timer_source(timer_asserted) {
            // SAFETY: Publication has not occurred, hardware is active, and
            // the caller still owns this stopped execution with IRQs masked.
            let rollback = unsafe {
                crate::hal::vm::deactivate_hardware(
                    &mut execution.hardware,
                    execution.vcpu_id,
                    interrupts,
                    crate::time::monotonic_ticks(),
                )
            };
            match rollback {
                Ok(()) => {
                    restore_reconcile_if_claimed(execution, reconcile_claimed);
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: vCPU activation could not program guest timer source: {timer:?}"
                    ));
                }
                Err(hardware) => fatal_ambiguous_hardware(
                    "timer rollback could not detach vCPU hardware",
                    hardware,
                ),
            };
        }
    }
    // A publication after the pre-admission claim must survive until IRQ-tail
    // detachment. In-place LR refresh cannot satisfy a remote quiescence
    // request, so there is no second claim or refresh while resident. The
    // publisher prompts the scheduler-authoritative CPU even during entry.
    // SAFETY: All temporary references ended; the scheduler-origin pointer
    // remains pinned and exclusive for the active run.
    if let Err(publication) = unsafe { super::active_vcpu::set_raw(execution, execution_claim) } {
        let publication_error = publication.error();
        execution_claim = publication.into_claim();
        // SAFETY: Publication failed, so no callback can borrow this execution.
        let execution = unsafe { &mut *execution };
        let interrupts = core::ptr::from_ref(execution.interrupts());
        // SAFETY: The VM binding retains the controller through rollback.
        let interrupts = unsafe { &*interrupts };
        // SAFETY: Hardware was activated above and IRQs remain masked.
        let rollback = unsafe {
            crate::hal::vm::deactivate_hardware(
                &mut execution.hardware,
                execution.vcpu_id,
                interrupts,
                crate::time::monotonic_ticks(),
            )
        };
        return match rollback {
            Ok(()) => match super::timer::deactivate_guest_timer_source() {
                Ok(()) => {
                    restore_reconcile_if_claimed(execution, consumed_reconcile);
                    release_execution_or_fail(execution, execution_claim);
                    Err(HardwareTransitionError::Active(publication_error))
                }
                Err(timer) => {
                    restore_reconcile_if_claimed(execution, consumed_reconcile);
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: publication rollback detached hardware but could not mask guest timer source: publication={publication_error:?}, timer={timer:?}"
                    ));
                }
            },
            Err(hardware) => fatal_ambiguous_hardware(
                "publication rollback could not detach vCPU hardware",
                hardware,
            ),
        };
    }
    // Refilling can retire a listed SPI and move its pending delivery to a
    // different vCPU. Publish those prompts before entering guest execution;
    // waiting until this vCPU next detaches could strand a remote WFI waiter.
    // SAFETY: Publication succeeded, no callback borrow is live, and local
    // interrupts remain masked while the pinned owner is borrowed immutably.
    if let Some(binding) = unsafe { &*execution }.vm_binding() {
        binding.publish_changed_interrupts();
    }
    Ok(())
}

/// Removes active publication before saving and detaching local hardware.
///
/// # Safety
///
/// `execution` must exclusively own the active local vCPU and local interrupts
/// must remain masked throughout this transaction.
pub(crate) unsafe fn deactivate(
    execution: &mut super::VcpuExecution,
) -> Result<(), HardwareTransitionError> {
    let claim = super::active_vcpu::clear(execution).map_err(HardwareTransitionError::Active)?;
    let interrupts = core::ptr::from_ref(execution.interrupts());
    // SAFETY: The retained VM binding keeps the controller fixed and live.
    let interrupts = unsafe { &*interrupts };
    // SAFETY: Publication is gone, guest execution stopped, and IRQs are masked.
    if let Err(error) = unsafe {
        crate::hal::vm::deactivate_hardware(
            &mut execution.hardware,
            execution.vcpu_id,
            interrupts,
            crate::time::monotonic_ticks(),
        )
    } {
        // Active callback publication is already gone, but local hardware may
        // still refer to this execution. Retain its claim and stop globally.
        fatal_ambiguous_hardware("could not detach active vCPU hardware", error);
    }
    // Local IRQs remain masked until the now-ownerless PPI is disabled. Timer
    // register deactivation alone does not retire controller-side delivery.
    if let Err(error) = super::timer::deactivate_guest_timer_source() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: active vCPU detach could not mask guest timer source: {error:?}"
        ));
    }
    release_execution_or_fail(execution, claim);
    Ok(())
}

/// Detaches a vector-stopped vCPU and consumes its linear proof.
///
/// # Safety
///
/// `execution` must be the exact active local vCPU represented by `stopped`.
/// Local interrupts must remain masked throughout the transaction.
pub(super) unsafe fn detach_stopped(
    execution: &mut super::VcpuExecution,
    stopped: crate::hal::vm::StoppedVcpuRun,
) -> DetachedVcpuExecution {
    let claim = match super::active_vcpu::clear(execution) {
        Ok(claim) => claim,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: stopped vCPU callback publication could not be cleared: {error:?}"
        )),
    };
    let interrupts = core::ptr::from_ref(execution.interrupts());
    // SAFETY: The retained VM binding keeps the controller fixed and live.
    let interrupts = unsafe { &*interrupts };
    // SAFETY: Vector capture closed lower-world ownership and returned the
    // exact linear stopped proof while this vCPU and its IRQ mask stayed local.
    if let Err(failure) = unsafe {
        crate::hal::vm::deactivate_stopped_hardware(
            &mut execution.hardware,
            execution.vcpu_id,
            interrupts,
            crate::time::monotonic_ticks(),
            stopped,
        )
    } {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: could not detach stopped vCPU hardware: {:?}",
            failure.error()
        ));
    }
    let Some(cpu) = crate::cpu::current_index() else {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: stopped vCPU detach lost its registered CPU"
        ));
    };
    DetachedVcpuExecution {
        execution: core::ptr::NonNull::from(execution),
        claim,
        cpu,
        armed: true,
        not_send_or_sync: core::marker::PhantomData,
    }
}

/// Linear interval after stopped hardware detach but before claim release.
#[must_use = "stopped-vCPU policy must complete before execution release"]
pub(super) struct DetachedVcpuExecution {
    execution: core::ptr::NonNull<super::VcpuExecution>,
    claim: Option<super::registry::VmExecutionClaim>,
    cpu: hyper::cpu::CpuIndex,
    armed: bool,
    not_send_or_sync: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

impl DetachedVcpuExecution {
    /// Borrows only the installed VM binding while its execution claim is held.
    pub(super) fn vm_binding(&self) -> Option<&super::registry::VmBinding> {
        // SAFETY: Construction retained the pinned execution and the armed
        // token prevents claim release or payload destruction during borrow.
        unsafe { self.execution.as_ref() }.vm_binding()
    }

    pub(super) fn wfi_state(
        &self,
    ) -> Result<crate::hal::vm::VcpuWfiState, crate::hal::vm::StoppedVcpuQueryError> {
        // SAFETY: The armed token retains exclusive ownership of this stopped
        // execution and its hardware state until `finish` consumes the token.
        let execution = unsafe { self.execution.as_ref() };
        crate::hal::vm::stopped_wfi_state(
            &execution.hardware,
            execution.vcpu_id,
            execution.interrupts(),
            crate::time::monotonic_ticks(),
        )
    }

    /// Masks guest timer delivery and releases the exact retained execution claim.
    pub(super) fn finish(mut self) {
        if crate::cpu::current_index() != Some(self.cpu) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: detached vCPU migrated before claim release"
            ));
        }
        if let Err(error) = super::timer::deactivate_guest_timer_source() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: stopped vCPU detach could not mask guest timer source: {error:?}"
            ));
        }
        // SAFETY: CPU identity matched and this armed token retains the exact
        // exclusive execution pointer until release completes.
        let execution = unsafe { self.execution.as_mut() };
        release_execution_or_fail(execution, self.claim.take());
        self.armed = false;
    }
}

impl Drop for DetachedVcpuExecution {
    fn drop(&mut self) {
        if self.armed {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: detached vCPU proof was dropped before claim release"
            ));
        }
    }
}

fn release_execution_or_fail(
    execution: &mut super::VcpuExecution,
    mut claim: Option<super::registry::VmExecutionClaim>,
) {
    if crate::hal::vm::take_reused_interrupt_save(&mut execution.hardware) {
        crate::pr_warn!(
            "HypeR: vCPU {} cleanup reused saved interrupt state after an incomplete local transition",
            execution.vcpu_id
        );
    }
    let Some(cpu) = crate::cpu::current_index() else {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: vCPU execution capability release has no registered CPU"
        ));
    };
    let Some(mut claim) = claim.take() else {
        if execution.vm_binding().is_some() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: installed vCPU lost its active execution capability"
            ));
        }
        return;
    };
    let Some(binding) = execution.vm_binding() else {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: active execution capability lost its VM binding"
        ));
    };
    if let Some(residency) = claim.take_residency()
        && let Err(failure) = super::memory::leave(binding, residency)
    {
        let error = failure.error();
        claim.restore_residency(failure.into_claim());
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: guest residency capability release failed: {error:?}"
        ));
    }
    if let Err(failure) = binding.release_execution(claim, cpu) {
        let error = failure.error();
        let _retained_claim = failure.into_claim();
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: VM execution capability release failed: {error:?}"
        ));
    }
    binding.publish_changed_interrupts();
}

fn restore_reconcile_if_claimed(execution: &super::VcpuExecution, claimed: bool) {
    if !claimed {
        return;
    }
    let Some(binding) = execution.vm_binding() else {
        hyper::debug::invariant_failure(
            "vm::vcpu::transition::restore_reconcile_if_claimed invariant",
        )
    };
    if binding
        .restore_interrupt_reconcile(execution.vcpu_id)
        .is_err()
    {
        hyper::debug::invariant_failure(
            "vm::vcpu::transition::restore_reconcile_if_claimed invariant",
        )
    }
}

fn claim_execution(
    execution: &super::VcpuExecution,
    cpu: hyper::cpu::CpuIndex,
) -> Result<Option<super::registry::VmExecutionClaim>, HardwareTransitionError> {
    match execution.vm_binding() {
        Some(binding) => binding
            .claim_execution(cpu)
            .map(Some)
            .map_err(HardwareTransitionError::Execution),
        None => Ok(None),
    }
}

fn fatal_ambiguous_hardware(
    operation: &'static str,
    error: crate::hal::vm::VcpuInterruptError,
) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {operation}: {error:?}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HardwareTransitionError {
    InterruptGateClosed,
    Active(super::active_vcpu::Error),
    Hardware(crate::hal::vm::VcpuInterruptError),
    Execution(super::registry::VmExecutionError),
    InvalidExecution,
    Memory(super::memory::Error),
    Registry(super::registry::Error),
}

pub(crate) fn current_interrupt_reconcile_pending() -> Result<bool, super::ReconcileObservationError>
{
    match super::active_vcpu::with(|execution| {
        let Some(binding) = execution.vm_binding() else {
            return Ok(false);
        };
        binding.interrupt_reconcile_pending(execution.vcpu_id)
    }) {
        Ok(Some(Ok(pending))) => Ok(pending),
        Ok(Some(Err(error))) => Err(super::ReconcileObservationError::Registry(error)),
        Ok(None) => Ok(false),
        Err(error) => Err(super::ReconcileObservationError::Active(error)),
    }
}

pub(crate) fn current_administrative_stop_requested()
-> Result<bool, super::ReconcileObservationError> {
    let current = crate::task::scheduler::current_vcpu_if_present()
        .map_err(super::ReconcileObservationError::Scheduler)?;
    let Some(current) = current else {
        return Ok(false);
    };
    match super::active_vcpu::with(|execution| {
        let Some(binding) = execution.vm_binding() else {
            return Ok(false);
        };
        binding
            .administrative_stop_requested(execution.vcpu_id, current.thread)
            .map(|reason| reason.is_some())
    }) {
        Ok(Some(Ok(pending))) => Ok(pending),
        Ok(Some(Err(error))) => Err(super::ReconcileObservationError::Registry(error)),
        Ok(None) => Ok(false),
        Err(error) => Err(super::ReconcileObservationError::Active(error)),
    }
}
