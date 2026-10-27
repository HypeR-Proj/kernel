// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest architectural-timer host binding.
//!
//! VM policy owns the physical PPI handler used to inject a guest timer. Host
//! timekeeping supplies only the firmware-derived source description.

pub(crate) mod recovery;

use hyper::cpu::PerCpu;
use hyper::hal::interrupt::{
    HostInterruptBinding, InterruptId, InterruptPriority, InterruptTrigger,
};
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use {hyper_platform::PlatformInterrupt, hyper_platform::PlatformInterruptTrigger};

use {crate::vm::timer::recovery::Recovery, crate::vm::timer::recovery::Update};

use {
    crate::irq::interrupt::HandlerResult, crate::irq::interrupt::IrqDomainId,
    crate::irq::interrupt::PreparedRegistration, crate::irq::interrupt::VirtualInterrupt,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Interrupt(crate::irq::interrupt::Error),
    InvalidHandlerContext,
    InvalidCpu,
    RecoveryGenerationExhausted,
    Time(crate::time::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValidationError {
    Allocation,
    Active(super::active_vcpu::Error),
    Hardware(super::vcpu::HardwareTransitionError),
    Interrupts(crate::hal::vm::InterruptError),
    Machine(crate::hal::vm::TimerValidationError),
    StateMismatch,
}

const NO_HOST_TIMER: u32 = u32::MAX;
static HOST_TIMER_INTERRUPT: AtomicU32 = AtomicU32::new(NO_HOST_TIMER);
static TIMER_SOURCE_MASKED: PerCpu<AtomicBool> =
    PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);
static RECOVERY: PerCpu<InterruptSpinLock<Recovery, crate::hal::irq::LocalMask>> =
    PerCpu::new([const { InterruptSpinLock::new(Recovery::new()) }; hyper::cpu::MAX_CPUS]);

pub(super) struct PreparedBinding {
    mapping: Option<PreparedRegistration>,
    maintenance: Option<PreparedRegistration>,
}

impl PreparedBinding {
    pub(super) const fn host_interrupt(&self) -> Option<HostInterruptBinding> {
        match self.mapping.as_ref() {
            Some(prepared) => Some(HostInterruptBinding::new(prepared.interrupt().get())),
            None => None,
        }
    }

    pub(super) const fn maintenance_interrupt(&self) -> Option<VirtualInterrupt> {
        match self.maintenance.as_ref() {
            Some(prepared) => Some(prepared.interrupt()),
            None => None,
        }
    }

    pub(super) fn activate(self) -> Result<(), Error> {
        let host_timer = self.host_interrupt().map(HostInterruptBinding::get);
        // Keep the timer-source and maintenance capabilities until their
        // complete group commits; neither can outlive a failed activation.
        let PreparedBinding {
            mapping: prepared_mapping,
            maintenance: prepared_maintenance,
        } = self;
        let mapping = match activate_one(prepared_mapping) {
            Ok(registration) => registration,
            Err(error) => {
                rollback_mapping(prepared_maintenance);
                return Err(error);
            }
        };
        let maintenance = match activate_one(prepared_maintenance) {
            Ok(registration) => registration,
            Err(error) => {
                rollback_active_mapping(mapping);
                return Err(error);
            }
        };
        if let Some(registration) = mapping {
            registration.retain_permanently();
        }
        if let Some(registration) = maintenance {
            registration.retain_permanently();
        }
        HOST_TIMER_INTERRUPT.store(host_timer.unwrap_or(NO_HOST_TIMER), Ordering::Release);
        Ok(())
    }

    pub(super) fn rollback(self) {
        rollback_mapping(self.mapping);
        rollback_mapping(self.maintenance);
    }
}

fn activate_one(
    prepared: Option<PreparedRegistration>,
) -> Result<Option<crate::irq::interrupt::Registration>, Error> {
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    match crate::irq::interrupt::activate(prepared) {
        Ok(registration) => Ok(Some(registration)),
        Err(failure) => {
            let (error, prepared) = failure.into_parts();
            if let Err(discard) = crate::irq::interrupt::discard_prepared(prepared) {
                let (discard_error, _prepared) = discard.into_parts();
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: guest timer IRQ activation rollback failed: {discard_error:?}"
                ));
            }
            Err(Error::Interrupt(error))
        }
    }
}

fn rollback_active_mapping(registration: Option<crate::irq::interrupt::Registration>) {
    let Some(registration) = registration else {
        return;
    };
    crate::hal::vm::quiesce_virtual_interrupt_delivery();
    let interrupt = registration.interrupt();
    match crate::irq::interrupt::unregister(registration) {
        Ok(()) => {
            if let Err(error) = crate::irq::interrupt::unmap(interrupt) {
                crate::pr_warn!("HypeR: active guest timer IRQ unmap failed: {error:?}");
            }
        }
        Err(failure) => {
            let (error, registration) = failure.into_parts();
            crate::pr_warn!(
                "HypeR: retaining active guest timer IRQ after rollback failed: {error:?}"
            );
            registration.retain_permanently();
        }
    }
}

/// Begins a new local hardware owner without inheriting its predecessor's retry.
/// Activation precedes active-vCPU publication while local IRQs remain masked.
pub(crate) fn activate_guest_timer_source(asserted: bool) -> Result<(), Error> {
    if HOST_TIMER_INTERRUPT.load(Ordering::Acquire) == NO_HOST_TIMER {
        return Ok(());
    }
    reset_recovery(true)?;
    set_guest_timer_source_enabled(!asserted)
}

/// Invalidates any retry before the detached hardware owner can be replaced.
pub(crate) fn deactivate_guest_timer_source() -> Result<(), Error> {
    if HOST_TIMER_INTERRUPT.load(Ordering::Acquire) == NO_HOST_TIMER {
        return Ok(());
    }
    reset_recovery(false)?;
    set_guest_timer_source_enabled(false)
}

fn reset_recovery(active: bool) -> Result<(), Error> {
    let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
    let (pending, update) =
        RECOVERY[cpu].with(|state| (state.is_pending(), state.change_owner(active)));
    let update = update.ok_or(Error::RecoveryGenerationExhausted)?;
    if pending {
        publish_recovery(update)
    } else {
        Ok(())
    }
}

fn publish_recovery(update: Update) -> Result<(), Error> {
    let deadline = update
        .delay_ns
        .map(crate::time::deadline_after)
        .transpose()
        .map_err(Error::Time)?;
    crate::time::publish_guest_timer_recovery(update.generation, deadline).map_err(Error::Time)
}

fn set_guest_timer_source_enabled(enabled: bool) -> Result<(), Error> {
    let interrupt = HOST_TIMER_INTERRUPT.load(Ordering::Acquire);
    if interrupt == NO_HOST_TIMER {
        return Ok(());
    }
    let interrupt = VirtualInterrupt::from_raw(interrupt);
    let result = if enabled {
        crate::irq::interrupt::enable_local(interrupt)
    } else {
        crate::irq::interrupt::disable_local(interrupt)
    };
    result.map_err(Error::Interrupt)?;
    set_local_source_masked(!enabled);
    Ok(())
}

pub(super) fn validate_hardware(
    timer_interrupt: hyper::hal::interrupt::InterruptId,
    services: &crate::hal::vm::ExitServicesReady,
    prepared_interrupts: &crate::hal::vm::PreparedInterruptVirtualization,
) -> Result<bool, ValidationError> {
    let now = crate::time::monotonic_ticks();
    let Some(prepared) = crate::hal::vm::prepare_timer_validation(
        timer_interrupt,
        now,
        services,
        prepared_interrupts,
    )
    .map_err(ValidationError::Machine)?
    else {
        return Ok(false);
    };
    let (capability, interrupts, hardware) = prepared.into_parts();
    let mut execution = super::vcpu::VcpuExecution::try_timer_validation(hardware, interrupts)
        .map_err(|_| ValidationError::Allocation)?;
    let execution_pointer = core::ptr::from_mut(execution.as_mut());
    // SAFETY: the owned allocation stays live through deactivation below;
    // validation is exclusive and local interrupts remain masked.
    unsafe { super::vcpu::activate(execution_pointer) }.map_err(ValidationError::Hardware)?;
    let validation = (|| {
        super::active_vcpu::with(|execution| {
            let (hardware, vcpu_id, interrupts) = execution.interrupt_context();
            crate::hal::vm::inject_timer_for_validation(&capability, hardware, vcpu_id, interrupts)
        })
        .map_err(ValidationError::Active)?
        .ok_or(ValidationError::StateMismatch)?
        .map_err(|_| ValidationError::StateMismatch)?;
        if !crate::hal::vm::timer_validation_succeeded(&capability, execution.interrupts())
            .map_err(ValidationError::Interrupts)?
        {
            return Err(ValidationError::StateMismatch);
        }
        Ok(())
    })();
    // SAFETY: This remains the active validation vCPU with IRQs masked.
    match unsafe { super::vcpu::deactivate(&mut execution) } {
        Ok(()) => {}
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: timer validation cannot retire active vCPU hardware: {error:?}"
        )),
    }
    validation.map(|()| true)
}

fn rollback_mapping(mapping: Option<PreparedRegistration>) {
    let Some(prepared) = mapping else {
        return;
    };
    if let Err(failure) = crate::irq::interrupt::discard_prepared(prepared) {
        let (error, _prepared) = failure.into_parts();
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: guest timer IRQ preparation rollback failed: {error:?}"
        ));
    }
}

pub(super) fn prepare(
    source: crate::time::GuestTimerSource,
    domain: IrqDomainId,
    maintenance: Option<PlatformInterrupt>,
) -> Result<PreparedBinding, Error> {
    let mapping = if source.requires_host_mapping {
        Some(
            domain
                .prepare_shared_mapping(
                    source.interrupt,
                    InterruptPriority::Normal,
                    InterruptTrigger::Level,
                    0,
                    handle_interrupt,
                )
                .map_err(Error::Interrupt)?,
        )
    } else {
        None
    };
    // Zero represents an absent timer mapping, so encode an allocated VIRQ as
    // raw + 1. VIRQ zero is valid and must not collide with the sentinel.
    let maintenance_context = match mapping.as_ref() {
        Some(prepared) => usize::try_from(prepared.interrupt().get())
            .ok()
            .and_then(|raw| raw.checked_add(1))
            .ok_or(Error::InvalidHandlerContext),
        None => Ok(0),
    };
    let maintenance_context = match maintenance_context {
        Ok(context) => context,
        Err(error) => {
            rollback_mapping(mapping);
            return Err(error);
        }
    };
    let maintenance = match maintenance {
        Some(interrupt) => match domain.prepare_shared_mapping(
            InterruptId::new(interrupt.interrupt),
            InterruptPriority::High,
            match interrupt.trigger {
                PlatformInterruptTrigger::Level => InterruptTrigger::Level,
                PlatformInterruptTrigger::Edge => InterruptTrigger::Edge,
            },
            maintenance_context,
            handle_maintenance_interrupt,
        ) {
            Ok(mapping) => Some(mapping),
            Err(error) => {
                rollback_mapping(mapping);
                return Err(Error::Interrupt(error));
            }
        },
        None => None,
    };
    Ok(PreparedBinding {
        mapping,
        maintenance,
    })
}

/// Completes controller mutation before scheduler notification. The active
/// execution borrow and architecture/controller locks all end before draining
/// cross-vCPU prompts. A concurrently unpublished VM is already retiring all
/// endpoints, so its registry lookup needs no further delivery work.
fn update_active_interrupts(
    update: fn(
        &mut crate::hal::vm::VcpuHardwareState,
        u32,
        &super::VmInterruptController,
    ) -> Result<bool, crate::hal::vm::VcpuInterruptError>,
) -> Result<Option<Result<bool, crate::hal::vm::VcpuInterruptError>>, super::active_vcpu::Error> {
    let observed = super::active_vcpu::with(|execution| {
        let id = execution.vm_binding().map(super::registry::VmBinding::id);
        let (hardware, vcpu_id, interrupts) = execution.interrupt_context();
        (id, update(hardware, vcpu_id, interrupts))
    })?;
    let Some((id, result)) = observed else {
        return Ok(None);
    };
    if let Some(id) = id {
        let _ = super::registry::with_binding(
            id,
            super::registry::VmBinding::publish_changed_interrupts,
        );
    }
    Ok(Some(result))
}

fn handle_interrupt(
    _interrupt: VirtualInterrupt,
    _context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    match update_active_interrupts(crate::hal::vm::handle_virtual_timer_interrupt) {
        Ok(Some(Ok(true))) => {
            set_local_source_masked(true);
            HandlerResult::HandledAndMaskLocal
        }
        Ok(Some(Ok(false))) => HandlerResult::Handled,
        Ok(None) => {
            set_local_source_masked(true);
            crate::pr_warn!("HypeR: masked virtual timer PPI without an active vCPU");
            HandlerResult::HandledAndMaskLocal
        }
        Ok(Some(Err(error))) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: virtual timer injection failed: {error:?}");
            HandlerResult::HandledAndMaskLocal
        }
        Err(error) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: invalid active vCPU during virtual timer IRQ: {error:?}");
            HandlerResult::HandledAndMaskLocal
        }
    }
}

fn handle_maintenance_interrupt(
    _interrupt: VirtualInterrupt,
    context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    if !crate::hal::vm::maintenance_interrupt_pending() {
        return HandlerResult::NotHandled;
    }
    let timer_eoi = match super::active_vcpu::with(|execution| {
        let (hardware, _, interrupts) = execution.interrupt_context();
        crate::hal::vm::virtual_timer_eoi_pending(hardware, interrupts)
    }) {
        Ok(Some(timer_eoi)) => timer_eoi,
        Ok(None) => false,
        Err(error) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: invalid active vCPU while observing timer EOI: {error:?}");
            return HandlerResult::Handled;
        }
    };
    match update_active_interrupts(crate::hal::vm::handle_maintenance_interrupt) {
        Ok(Some(Ok(true))) => {
            let Some(raw) = context
                .checked_sub(1)
                .and_then(|raw| u32::try_from(raw).ok())
            else {
                crate::hal::vm::quiesce_virtual_interrupt_delivery();
                crate::pr_err!("HypeR: invalid virtual-timer maintenance binding");
                return HandlerResult::Handled;
            };
            finish_recovery();
            set_local_source_masked(false);
            HandlerResult::HandledAndUnmaskLocal(VirtualInterrupt::from_raw(raw))
        }
        Ok(Some(Ok(false))) => {
            if timer_eoi && local_source_masked() {
                start_recovery();
            }
            HandlerResult::Handled
        }
        Ok(None) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: virtual-interrupt maintenance without an active vCPU");
            HandlerResult::Handled
        }
        Ok(Some(Err(error))) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: virtual-interrupt maintenance failed: {error:?}");
            HandlerResult::Handled
        }
        Err(error) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: invalid active vCPU during maintenance IRQ: {error:?}");
            HandlerResult::Handled
        }
    }
}

fn start_recovery() {
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    if let Some(update) = RECOVERY[cpu].with(Recovery::start) {
        publish_recovery_or_fail(update);
    }
}

fn finish_recovery() {
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    if let Some(update) = RECOVERY[cpu].with(Recovery::complete) {
        publish_recovery_or_fail(update);
    }
}

fn publish_recovery_or_fail(update: Update) {
    if let Err(error) = publish_recovery(update) {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: guest timer recovery deadline failed: {error:?}"
        ));
    }
}

/// Rechecks only an observed early timer EOI, outside the IRQ registry lock.
///
/// The timer deassertion has no interrupt edge of its own. One-shot retries
/// back off to a bounded rate until it deasserts; unrelated CPUs and inactive
/// guests have no recovery timer. Every hardware detach revokes this generation
/// before another owner can occupy the local virtual timer bank.
pub(crate) fn recover_source(generation: u64) {
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    if !RECOVERY[cpu].with(|state| state.accepts(generation)) {
        return;
    }
    let can_unmask = match update_active_interrupts(crate::hal::vm::handle_maintenance_interrupt) {
        Ok(Some(Ok(can_unmask))) => can_unmask,
        // The physical timer bank is detached. Keep its PPI masked until
        // activation installs an owner, even if a prior assertion is pending
        // at the interrupt controller after the timer was disabled.
        Ok(None) => {
            finish_recovery();
            return;
        }
        Ok(Some(Err(error))) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: virtual-timer recovery failed: {error:?}");
            finish_recovery();
            return;
        }
        Err(error) => {
            crate::hal::vm::quiesce_virtual_interrupt_delivery();
            crate::pr_err!("HypeR: invalid active vCPU during timer recovery: {error:?}");
            finish_recovery();
            return;
        }
    };
    if can_unmask {
        finish_recovery();
        if let Err(error) = set_guest_timer_source_enabled(true) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest timer recovery cannot unmask its source: {error:?}"
            ));
        }
    } else {
        if let Some(update) = RECOVERY[cpu].with(|state| state.retry(generation)) {
            publish_recovery_or_fail(update);
        }
    }
}

fn local_source_masked() -> bool {
    crate::cpu::current_index().is_some_and(|cpu| TIMER_SOURCE_MASKED[cpu].load(Ordering::Relaxed))
}

fn set_local_source_masked(masked: bool) {
    if let Some(cpu) = crate::cpu::current_index() {
        TIMER_SOURCE_MASKED[cpu].store(masked, Ordering::Relaxed);
    }
}
