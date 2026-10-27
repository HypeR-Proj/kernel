// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Running-vCPU bridge for the Arm virtual timer and vGIC.

use hyper_vm_models::arm::gic::RuntimeError as VgicError;
use hyper_vm_models::interrupt::VirtualCpuId;

use crate::arch::aarch64::VmInterruptController;
use {
    crate::arch::aarch64::vm::vgic, crate::arch::aarch64::vm::vgic::Error as VgicArchitectureError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    VgicArchitecture(VgicArchitectureError),
    VgicModel(VgicError),
}

impl From<VgicArchitectureError> for Error {
    fn from(error: VgicArchitectureError) -> Self {
        Self::VgicArchitecture(error)
    }
}

impl From<VgicError> for Error {
    fn from(error: VgicError) -> Self {
        Self::VgicModel(error)
    }
}

pub fn reconcile_saved(
    context: &mut crate::arch::aarch64::VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<(), Error> {
    let asserted = context.virtual_timer_interrupt_asserted_at(physical_count);
    update_model(vcpu_id, interrupts, asserted)
}

pub fn refresh_running_vgic(
    context: &mut crate::arch::aarch64::VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    timer_asserted: bool,
) -> Result<(), Error> {
    // SAFETY: The active binding identifies the vCPU whose hardware interface
    // is loaded on this CPU, and IRQ entry has masked local interrupts.
    if let Err(error) = unsafe { context.deactivate_vgic() } {
        vgic::disable();
        return Err(error.into());
    }
    let result = interrupts.with(|controller| {
        let vcpu = VirtualCpuId::new(vcpu_id);
        controller.synchronize(vcpu, context.vgic.slots())?;
        set_timer_level(controller, interrupts, vcpu, timer_asserted)?;
        let _ = controller.refill_current(vcpu, context.vgic.slots_mut())?;
        Ok::<(), VgicError>(())
    });
    result.inspect_err(|_| {
        vgic::disable();
    })?;
    // SAFETY: The complete model has been reconciled into this vCPU's slots.
    unsafe { context.activate_vgic()? };
    Ok(())
}

fn update_model(
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    asserted: bool,
) -> Result<(), Error> {
    interrupts
        .with(|controller| {
            set_timer_level(controller, interrupts, VirtualCpuId::new(vcpu_id), asserted)
        })
        .map_err(Into::into)
}

fn set_timer_level(
    controller: &mut hyper_vm_models::arm::gic::VirtualGic,
    interrupts: &VmInterruptController,
    vcpu: VirtualCpuId,
    asserted: bool,
) -> Result<(), VgicError> {
    controller.set_line(interrupts.timer_interrupt(), vcpu, asserted)
}
