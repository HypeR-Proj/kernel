// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` vCPU timer and virtual-interrupt hardware-state mechanisms.

use hyper_vm_models::arm::gic::GicInterruptId;
use hyper_vm_models::interrupt::VirtualCpuId;

use {
    crate::arch::aarch64::VcpuContext, crate::arch::aarch64::VmInterruptController,
    crate::arch::aarch64::vm::timer as vm_timer,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InterruptGateClosed,
    Architecture(crate::arch::aarch64::VgicError),
    Bridge(vm_timer::Error),
    Controller(hyper_vm_models::arm::gic::RuntimeError),
    GuestRun(crate::arch::aarch64::context::GuestRunError),
    ReturnWorld(crate::arch::aarch64::lower_el::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GicAccessError {
    Architecture(crate::arch::aarch64::VgicError),
    Transaction(crate::arch::aarch64::vm::interrupt::AccessError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializationError {
    MissingHostTimerInterrupt,
}

impl From<crate::arch::aarch64::VgicError> for Error {
    fn from(error: crate::arch::aarch64::VgicError) -> Self {
        Self::Architecture(error)
    }
}

impl From<hyper_vm_models::arm::gic::RuntimeError> for Error {
    fn from(error: hyper_vm_models::arm::gic::RuntimeError) -> Self {
        Self::Controller(error)
    }
}

impl From<vm_timer::Error> for Error {
    fn from(error: vm_timer::Error) -> Self {
        Self::Bridge(error)
    }
}

impl From<crate::arch::aarch64::lower_el::Error> for Error {
    fn from(error: crate::arch::aarch64::lower_el::Error) -> Self {
        Self::ReturnWorld(error)
    }
}

/// Loads the timer, system-register, and virtual-interrupt state of a stopped
/// vCPU into the current PE.
///
/// # Safety
///
/// `context` must be pinned and exclusively owned by the stopped vCPU. No
/// virtual hardware may already be active locally, stage-2 must already select
/// this VM, and local interrupts must remain masked.
pub unsafe fn activate(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<bool, Error> {
    if !interrupts.try_enter(VirtualCpuId::new(vcpu_id)) {
        return Err(Error::InterruptGateClosed);
    }
    // SAFETY: The caller owns this detached context; admission above keeps
    // a concurrent quiesce transaction from completing until we detach.
    let result = unsafe { activate_admitted(context, vcpu_id, interrupts, physical_count) };
    if result.is_err() {
        // No guest instruction ran. The saved model already describes every
        // LR loaded by the failed activation; release the entry reservation.
        interrupts.abandon_entry(VirtualCpuId::new(vcpu_id));
    }
    result
}

unsafe fn activate_admitted(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<bool, Error> {
    vm_timer::reconcile_saved(context, vcpu_id, interrupts, physical_count)?;
    let timer_asserted = context.virtual_timer_interrupt_asserted_at(physical_count);

    // SAFETY: The caller owns this stopped vCPU and IRQs remain masked.
    unsafe { context.activate_system_registers() };
    // Keep the direct-read virtual identity coherent even though the current
    // TID3 policy normally routes MIDR/MPIDR through software emulation.
    crate::arch::aarch64::vm::sysreg::activate_virtual_identity(vcpu_id);
    // SAFETY: The same exclusive stopped-vCPU contract covers its timer.
    unsafe { context.activate_timer() };
    let result = interrupts.with(|controller| {
        let vcpu = VirtualCpuId::new(vcpu_id);
        controller.synchronize(vcpu, context.vgic.slots())?;
        let _ = controller.refill_current(vcpu, context.vgic.slots_mut())?;
        Ok::<(), hyper_vm_models::arm::gic::RuntimeError>(())
    });
    if let Err(error) = result {
        // SAFETY: Activation above made this the live local timer bank.
        unsafe { context.deactivate_timer() };
        // SAFETY: Guest execution never began and this context still owns the
        // live local system-register bank.
        unsafe { context.deactivate_system_registers() };
        return Err(error.into());
    }
    // SAFETY: Model synchronization completed while this stopped vCPU remained
    // exclusively owned with local IRQs masked.
    if let Err(error) = unsafe { context.activate_vgic() } {
        crate::arch::aarch64::disable_vgic();
        // SAFETY: The timer was activated above and remains local.
        unsafe { context.deactivate_timer() };
        // SAFETY: Guest execution never began and this context owns the live
        // system-register bank.
        unsafe { context.deactivate_system_registers() };
        return Err(error.into());
    }
    if let Err(error) = crate::arch::aarch64::lower_el::publish_guest(context) {
        crate::arch::aarch64::disable_vgic();
        // SAFETY: Guest execution did not begin and this context still owns
        // the local timer and system-register banks activated above.
        unsafe { context.deactivate_timer() };
        // SAFETY: The failed publication left no lower-EL frame consumer.
        unsafe { context.deactivate_system_registers() };
        return Err(error.into());
    }
    Ok(timer_asserted)
}

/// Saves and detaches the active local timer, vGIC, and system-register state.
///
/// # Safety
///
/// `context` must exclusively own the active local vCPU, guest execution must
/// have stopped, and local interrupts must remain masked.
pub unsafe fn deactivate(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<(), Error> {
    // Retire return-world ownership before changing any live guest register
    // bank. A lower-EL vector can therefore never observe partially detached
    // hardware under a still-published guest identity.
    crate::arch::aarch64::lower_el::retire_guest(context)?;
    // SAFETY: The caller's active-vCPU contract remains in force after
    // return-world retirement.
    unsafe { deactivate_banks(context, vcpu_id, interrupts, physical_count) }
}

/// Detaches a terminal run whose vector already closed lower-EL ownership.
///
/// # Safety
///
/// `context` and `stopped` must identify the same active local vCPU. Local
/// interrupts remain masked and callback publication must already be cleared.
pub unsafe fn deactivate_stopped(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
    mut stopped: crate::arch::aarch64::context::StoppedGuestRun,
) -> Result<(), StoppedDeactivationFailure> {
    // SAFETY: The caller owns this terminal stopped vCPU and its live banks.
    if let Err(error) = unsafe { deactivate_banks(context, vcpu_id, interrupts, physical_count) } {
        return Err(StoppedDeactivationFailure {
            error,
            _stopped: stopped,
        });
    }
    if let Err(error) = context.consume_stopped(&mut stopped) {
        return Err(StoppedDeactivationFailure {
            error: Error::GuestRun(error),
            _stopped: stopped,
        });
    }
    Ok(())
}

unsafe fn deactivate_banks(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<(), Error> {
    // SAFETY: This context owns the active local timer bank.
    unsafe { context.deactivate_timer() };
    // SAFETY: Guest execution is stopped and local interrupts are masked.
    let vgic_result = unsafe { context.deactivate_vgic() };
    if vgic_result.is_err() {
        crate::arch::aarch64::disable_vgic();
    }
    // SAFETY: This context owns the live guest system-register bank.
    unsafe { context.deactivate_system_registers() };
    vgic_result?;
    interrupts.synchronize_detached(VirtualCpuId::new(vcpu_id), context.vgic.slots())?;
    vm_timer::reconcile_saved(context, vcpu_id, interrupts, physical_count)?;
    Ok(())
}

pub struct StoppedDeactivationFailure {
    error: Error,
    _stopped: crate::arch::aarch64::context::StoppedGuestRun,
}

impl StoppedDeactivationFailure {
    pub const fn error(&self) -> Error {
        self.error
    }
}

pub(crate) fn deliver_software_interrupt_group(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    request: u64,
    group: Option<hyper_vm_models::arm::gic::InterruptGroup>,
) -> Result<(), Error> {
    const TARGET_LIST_MASK: u64 = 0xffff;
    const AFFINITY_1_SHIFT: u32 = 16;
    const INTERRUPT_SHIFT: u32 = 24;
    const AFFINITY_2_SHIFT: u32 = 32;
    const BROADCAST: u64 = 1 << 40;
    const RANGE_SHIFT: u32 = 44;
    const AFFINITY_3_SHIFT: u32 = 48;

    let interrupt = GicInterruptId::new(((request >> INTERRUPT_SHIFT) & 0xf) as u32).ok_or(
        Error::Controller(hyper_vm_models::arm::gic::RuntimeError::NotConfigured),
    )?;
    let target_list = request & TARGET_LIST_MASK;
    let affinity_1 = (request >> AFFINITY_1_SHIFT) & 0xff;
    let affinity_2 = (request >> AFFINITY_2_SHIFT) & 0xff;
    let affinity_3 = (request >> AFFINITY_3_SHIFT) & 0xff;
    let range = (request >> RANGE_SHIFT) & 0xf;
    let broadcast = request & BROADCAST != 0;

    // SAFETY: Guest synchronous entry masked local IRQs and this is the active vCPU.
    if let Err(error) = unsafe { context.deactivate_vgic() } {
        crate::arch::aarch64::disable_vgic();
        return Err(error.into());
    }
    let result = interrupts.with(|controller| {
        let current = VirtualCpuId::new(vcpu_id);
        controller.synchronize(current, context.vgic.slots())?;
        for index in 0..interrupts.vcpu_count() {
            let aff0 = u64::from(index & 0xff);
            let aff1 = u64::from((index >> 8) & 0xff);
            let aff2 = u64::from((index >> 16) & 0xff);
            let aff3 = u64::from((index >> 24) & 0xff);
            let selected = if broadcast {
                index != vcpu_id
            } else {
                aff1 == affinity_1
                    && aff2 == affinity_2
                    && aff3 == affinity_3
                    && aff0 >= range * 16
                    && aff0 < range * 16 + 16
                    && target_list & (1 << (aff0 - range * 16)) != 0
            };
            if selected
                && group.is_none_or(|group| {
                    controller
                        .snapshot(interrupt, VirtualCpuId::new(index))
                        .is_ok_and(|snapshot| snapshot.group == group)
                })
            {
                controller.inject(interrupt, VirtualCpuId::new(index))?;
            }
        }
        let _ = controller.refill_current(current, context.vgic.slots_mut())?;
        Ok::<(), hyper_vm_models::arm::gic::RuntimeError>(())
    });
    if let Err(error) = result {
        crate::arch::aarch64::disable_vgic();
        return Err(error.into());
    }
    // SAFETY: The complete model was reconciled into this still-active vCPU.
    unsafe { context.activate_vgic()? };
    Ok(())
}

pub fn handle_virtual_timer_interrupt(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
) -> Result<bool, Error> {
    let asserted = context.virtual_timer_interrupt_asserted_hardware();
    vm_timer::refresh_running_vgic(context, vcpu_id, interrupts, asserted)?;
    Ok(asserted)
}

pub fn handle_maintenance_interrupt(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
) -> Result<bool, Error> {
    let asserted = context.virtual_timer_interrupt_asserted_hardware();
    vm_timer::refresh_running_vgic(context, vcpu_id, interrupts, asserted)?;
    Ok(!asserted)
}

/// Identifies timer EOI maintenance before synchronizing the hardware LR bank.
/// An unrelated maintenance interrupt must not start a timer-source poll.
pub fn virtual_timer_eoi_pending(
    context: &VcpuContext,
    interrupts: &VmInterruptController,
) -> bool {
    let eoi = crate::arch::aarch64::vgic_maintenance_state().eoi_list_registers;
    context
        .vgic
        .slots()
        .iter()
        .enumerate()
        .any(|(index, entry)| {
            eoi & (1u64 << index) != 0
                && entry.is_some_and(|entry| entry.interrupt == interrupts.timer_interrupt())
        })
}

pub fn maintenance_interrupt_pending() -> bool {
    crate::arch::aarch64::vgic_maintenance_state().status != 0
}

pub fn quiesce_virtual_interrupt_delivery() {
    crate::arch::aarch64::disable_vgic();
}

pub fn inject_timer_for_validation(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
) -> Result<(), Error> {
    vm_timer::refresh_running_vgic(context, vcpu_id, interrupts, true).map_err(Into::into)
}

pub(crate) fn update_guest_device_interrupt(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    interrupt: GicInterruptId,
    asserted: bool,
) -> Result<(), Error> {
    // SAFETY: Guest-exit dispatch supplied the active local vCPU with IRQs masked.
    if let Err(error) = unsafe { context.deactivate_vgic() } {
        crate::arch::aarch64::disable_vgic();
        return Err(error.into());
    }
    let result = interrupts.with(|controller| {
        let vcpu = VirtualCpuId::new(vcpu_id);
        controller.synchronize(vcpu, context.vgic.slots())?;
        let target = if interrupt.get() >= 32 {
            controller.target(interrupt)?
        } else {
            vcpu
        };
        controller.set_line(interrupt, target, asserted)?;
        let _ = controller.refill_current(vcpu, context.vgic.slots_mut())?;
        Ok::<(), hyper_vm_models::arm::gic::RuntimeError>(())
    });
    if let Err(error) = result {
        crate::arch::aarch64::disable_vgic();
        return Err(error.into());
    }
    // SAFETY: Refill completed for the same active local vCPU.
    unsafe { context.activate_vgic()? };
    Ok(())
}

/// Updates only the VM-owned interrupt model for a stopped or remotely
/// running vCPU. The caller publishes durable reconcile work after this lock
/// transaction completes; this function never accesses another CPU's live
/// virtual-interface bank.
pub(crate) fn update_saved_guest_device_interrupt(
    interrupts: &VmInterruptController,
    vcpu_id: u32,
    interrupt: GicInterruptId,
    asserted: bool,
) -> Result<(), Error> {
    interrupts
        .with(|controller| {
            let vcpu = if interrupt.get() >= 32 {
                controller.target(interrupt)?
            } else {
                VirtualCpuId::new(vcpu_id)
            };
            controller.set_line(interrupt, vcpu, asserted)
        })
        .map_err(Into::into)
}

/// MSI delivery is a pending event, independent of the guest's trigger setting.
/// Publishing a transient level would lose the event when the guest has not
/// configured its SPI yet. Mutation and target selection share one model lock.
pub(crate) fn inject_saved_guest_device_interrupt(
    interrupts: &VmInterruptController,
    interrupt: GicInterruptId,
) -> Result<(), Error> {
    interrupts
        .with(|controller| {
            let target = controller.target(interrupt)?;
            controller.inject(interrupt, target)
        })
        .map_err(Into::into)
}

/// Requests a prompt exit from a guest currently running on `cpu`.
///
/// The VM-owned reconcile bit is the durable condition. The targeted SGI is
/// only a hardware prompt and may race harmlessly with migration.
pub(crate) fn request_guest_exit(cpu: hyper::cpu::CpuIndex) -> bool {
    crate::arch::aarch64::irq::gic::notify_guest_exit(cpu)
}

pub(crate) fn access_guest_gic(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    access: hyper_vm_models::arm::gic::mmio::DecodedAccess,
    operation: hyper_vm_models::exit::MmioOperation,
) -> Result<Option<u64>, GicAccessError> {
    // SAFETY: Guest synchronous entry masked local IRQs and owns this vCPU.
    if let Err(error) = unsafe { context.deactivate_vgic() } {
        crate::arch::aarch64::disable_vgic();
        return Err(GicAccessError::Architecture(error));
    }
    let split_eoi = context.vgic.split_eoi_mode();
    let result = interrupts.access_saved_bank(
        VirtualCpuId::new(vcpu_id),
        context.vgic.slots_mut(),
        access,
        operation,
        split_eoi,
    );
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            crate::arch::aarch64::disable_vgic();
            return Err(GicAccessError::Transaction(error));
        }
    };
    // SAFETY: The transaction reconciled and refilled the complete saved bank.
    if let Err(error) = unsafe { context.activate_vgic() } {
        crate::arch::aarch64::disable_vgic();
        return Err(GicAccessError::Architecture(error));
    }
    Ok(value)
}

/// Captures the current CPU-interface state before emulating trapped common
/// registers. Never read the host's physical ICC state on behalf of a guest.
pub(crate) fn access_common_register(
    context: &mut VcpuContext,
    encoding: crate::arch::aarch64::registers::SystemRegisterEncoding,
    write: Option<u64>,
) -> Result<Option<u64>, Error> {
    // SAFETY: Synchronous guest dispatch owns the local bank with IRQs masked.
    unsafe { context.deactivate_vgic()? };
    let result = match write {
        Some(value) => context.vgic.write_common(encoding, value).then_some(0),
        None => context.vgic.read_common(encoding),
    };
    // SAFETY: The same owner restores the captured bank with validated edits.
    unsafe { context.activate_vgic()? };
    Ok(result)
}

pub(crate) fn deactivate_guest_interrupt(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    value: u64,
    v2: bool,
) -> Result<bool, Error> {
    // SAFETY: The current trapped guest exclusively owns this masked CPU bank.
    unsafe { context.deactivate_vgic()? };
    let interrupt = if v2 {
        value as u32 & 0x3ff
    } else {
        value as u32 & 0xffffff
    };
    let source = v2.then_some(((value >> 10) & 7) as u8);
    let split = context.vgic.split_eoi_mode();
    let deferred = interrupts.deactivate_saved_bank(
        VirtualCpuId::new(vcpu_id),
        context.vgic.slots_mut(),
        interrupt,
        source,
        split,
    )?;
    // SAFETY: A deferred request still holds its entry reservation. The runner
    // subsequently saves/detaches this bank before the transaction can commit.
    unsafe { context.activate_vgic()? };
    Ok(deferred)
}
