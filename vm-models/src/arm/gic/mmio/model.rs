// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared guest GIC register state and interrupt-model effects.

use crate::interrupt::VirtualCpuId;
use {
    crate::arm::gic::GicInterruptId, crate::arm::gic::InterruptGroup,
    crate::arm::gic::InterruptSnapshot, crate::arm::gic::InterruptTrigger,
    crate::arm::gic::RuntimeError, crate::arm::gic::VirtualGic,
};

use super::decode::ModelRegisterKind;
use super::{BitmapRegister, ModelRegister, ServiceRegister};

const DISTRIBUTOR_CONTROL_MASK: u32 = (1 << 4) | (1 << 1) | 1;

/// Service-register state that must share serialization with interrupt state.
pub struct RegisterState {
    distributor_control: u32,
}

impl RegisterState {
    pub const fn new() -> Self {
        Self {
            distributor_control: 0,
        }
    }

    pub const fn read(&self, register: ServiceRegister) -> u64 {
        self.read_for_cpu(register, 0, 1)
    }

    pub const fn read_for_cpu(&self, register: ServiceRegister, cpu: u32, count: u32) -> u64 {
        match register {
            ServiceRegister::CpuDeactivateV2 => 0,
            ServiceRegister::DistributorControl | ServiceRegister::DistributorControlV2 => {
                self.distributor_control as u64
            }
            ServiceRegister::DistributorTypeV2 => {
                ((super::INTERRUPT_COUNT / 32 - 1) as u64) | (((count - 1) as u64) << 5)
            }
            ServiceRegister::PeripheralId2V2 => 0x20,
            ServiceRegister::DistributorType => {
                ((super::INTERRUPT_COUNT / 32 - 1) as u64) | (15 << 19)
            }
            // GICD_TYPER2 is optional and explicitly unimplemented.
            ServiceRegister::DistributorType2 => 0,
            ServiceRegister::DistributorImplementer | ServiceRegister::RedistributorImplementer => {
                0x43b
            }
            ServiceRegister::DistributorStatus | ServiceRegister::RedistributorStatus => 0,
            ServiceRegister::RedistributorControl | ServiceRegister::RedistributorWake => 0,
            ServiceRegister::RedistributorType => {
                ((cpu as u64) << 32)
                    | ((cpu as u64) << 8)
                    | if cpu + 1 == count { 1 << 4 } else { 0 }
            }
            ServiceRegister::PeripheralId2 => 0x30,
        }
    }

    pub fn write(&mut self, register: ServiceRegister, value: u64) {
        if register == ServiceRegister::DistributorControlV2 {
            self.distributor_control = value as u32 & 1;
        }
        if register == ServiceRegister::DistributorControl {
            self.distributor_control = value as u32 & DISTRIBUTOR_CONTROL_MASK;
        }
        // Identification, status, and current Redistributor service registers
        // are read-only or explicitly WI in this model.
    }
}

impl Default for RegisterState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    InvalidDecodedSpan,
    Interrupt(RuntimeError),
    UnsupportedRouteValue(u64),
}

impl From<RuntimeError> for ModelError {
    fn from(error: RuntimeError) -> Self {
        Self::Interrupt(error)
    }
}

pub fn read_model_register(
    controller: &VirtualGic,
    vcpu: VirtualCpuId,
    register: ModelRegister,
) -> Result<u64, ModelError> {
    match register.kind {
        ModelRegisterKind::PrivateEnableV2 { set } => read_bitmap(
            controller,
            vcpu,
            if set {
                BitmapRegister::SetEnable
            } else {
                BitmapRegister::ClearEnable
            },
            0,
        ),
        ModelRegisterKind::Bitmap {
            register,
            first_interrupt,
        } => read_bitmap(controller, vcpu, register, first_interrupt),
        ModelRegisterKind::Priority {
            first_interrupt,
            count,
            ..
        } => read_priority(controller, vcpu, first_interrupt, count),
        ModelRegisterKind::Configuration { first_interrupt } => {
            read_configuration(controller, vcpu, first_interrupt)
        }
        ModelRegisterKind::Route(route) => {
            Ok(controller.route_value(interrupt(route.interrupt())?)?)
        }
        ModelRegisterKind::SoftwareInterrupt => Ok(0),
        ModelRegisterKind::SgiPending {
            first_interrupt,
            count,
            ..
        } => {
            let mut value = 0;
            for byte in 0..count {
                value |= u64::from(
                    controller.sgi_sources(interrupt(first_interrupt + u32::from(byte))?, vcpu)?,
                ) << (byte * 8);
            }
            Ok(value)
        }
        ModelRegisterKind::Targets {
            first_interrupt,
            count,
        } => {
            let mut value = 0;
            for byte in 0..count {
                let id = first_interrupt + u32::from(byte);
                let mask = if id < 32 {
                    1u8 << vcpu.get()
                } else {
                    controller.target_mask(interrupt(id)?)?
                };
                value |= u64::from(mask) << (byte * 8);
            }
            Ok(value)
        }
    }
}

pub fn write_model_register(
    controller: &mut VirtualGic,
    vcpu: VirtualCpuId,
    register: ModelRegister,
    value: u64,
) -> Result<(), ModelError> {
    match register.kind {
        ModelRegisterKind::PrivateEnableV2 { set } => write_bitmap(
            controller,
            vcpu,
            if set {
                BitmapRegister::SetEnable
            } else {
                BitmapRegister::ClearEnable
            },
            0,
            value & 0xffff_0000,
        ),
        ModelRegisterKind::Bitmap {
            register,
            first_interrupt,
        } => write_bitmap(controller, vcpu, register, first_interrupt, value),
        ModelRegisterKind::Priority {
            first_interrupt,
            count,
            mask,
        } => write_priority(
            controller,
            vcpu,
            first_interrupt,
            count,
            value & (u64::from(mask) * 0x0101_0101),
        ),
        ModelRegisterKind::Configuration { first_interrupt } => {
            write_configuration(controller, vcpu, first_interrupt, value)
        }
        ModelRegisterKind::Targets {
            first_interrupt,
            count,
        } => {
            for byte in 0..count {
                let _ = snapshot(controller, vcpu, first_interrupt + u32::from(byte))?;
            }
            for byte in 0..count {
                let id = first_interrupt + u32::from(byte);
                if id >= 32 {
                    controller.set_target_mask(interrupt(id)?, (value >> (byte * 8)) as u8)?;
                }
            }
            Ok(())
        }
        ModelRegisterKind::SoftwareInterrupt => {
            let filter = (value >> 24) & 3;
            for cpu in 0..controller.vcpu_count() {
                let selected = match filter {
                    0 => value & (1 << (16 + cpu)) != 0,
                    1 => cpu != vcpu.get(),
                    2 => cpu == vcpu.get(),
                    _ => false,
                };
                if selected {
                    controller.inject_sgi(
                        interrupt(value as u32 & 15)?,
                        VirtualCpuId::new(cpu),
                        vcpu,
                    )?;
                }
            }
            Ok(())
        }
        ModelRegisterKind::SgiPending {
            first_interrupt,
            count,
            set,
        } => {
            for byte in 0..count {
                let id = interrupt(first_interrupt + u32::from(byte))?;
                let sources = (value >> (byte * 8)) as u8;
                if set {
                    for source in 0..controller.vcpu_count().min(8) {
                        if sources & (1 << source) != 0 {
                            controller.inject_sgi(id, vcpu, VirtualCpuId::new(source))?;
                        }
                    }
                } else {
                    controller.clear_sgi_sources(id, vcpu, sources)?;
                }
            }
            Ok(())
        }
        ModelRegisterKind::Route(route) => {
            controller.set_route_value(interrupt(route.interrupt())?, value)?;
            Ok(())
        }
    }
}

fn read_bitmap(
    controller: &VirtualGic,
    vcpu: VirtualCpuId,
    register: BitmapRegister,
    first_interrupt: u32,
) -> Result<u64, ModelError> {
    let mut value = 0;
    for bit in 0..32u32 {
        let snapshot = snapshot(controller, vcpu, lane(first_interrupt, bit)?)?;
        let set = match register {
            BitmapRegister::Group => snapshot.group == InterruptGroup::Group1,
            BitmapRegister::SetEnable | BitmapRegister::ClearEnable => snapshot.enabled,
            BitmapRegister::SetPending | BitmapRegister::ClearPending => snapshot.pending,
            BitmapRegister::SetActive | BitmapRegister::ClearActive => snapshot.active,
        };
        if set {
            value |= 1 << bit;
        }
    }
    Ok(value)
}

fn write_bitmap(
    controller: &mut VirtualGic,
    vcpu: VirtualCpuId,
    register: BitmapRegister,
    first_interrupt: u32,
    value: u64,
) -> Result<(), ModelError> {
    // Validate the complete mutation set before changing the first lane. The
    // controller is exclusively borrowed, so these entries cannot disappear
    // between this pass and the infallible-by-construction update pass.
    for bit in 0..32u32 {
        if register == BitmapRegister::Group || value & (1 << bit) != 0 {
            let _ = snapshot(controller, vcpu, lane(first_interrupt, bit)?)?;
        }
    }
    for bit in 0..32u32 {
        let set = value & (1 << bit) != 0;
        if register != BitmapRegister::Group && !set {
            continue;
        }
        let id = lane(first_interrupt, bit)?;
        let interrupt = interrupt(id)?;
        let target = target_for(controller, id, vcpu)?;
        match register {
            BitmapRegister::Group => controller.set_group(
                interrupt,
                target,
                if set {
                    InterruptGroup::Group1
                } else {
                    InterruptGroup::Group0
                },
            )?,
            BitmapRegister::SetEnable => controller.set_enabled(interrupt, target, true)?,
            BitmapRegister::ClearEnable => controller.set_enabled(interrupt, target, false)?,
            BitmapRegister::SetPending => controller.inject(interrupt, target)?,
            BitmapRegister::ClearPending => controller.clear_pending(interrupt, target)?,
            BitmapRegister::SetActive => controller.set_active(interrupt, target, true)?,
            BitmapRegister::ClearActive => controller.set_active(interrupt, target, false)?,
        }
    }
    Ok(())
}

fn read_priority(
    controller: &VirtualGic,
    vcpu: VirtualCpuId,
    first_interrupt: u32,
    count: u8,
) -> Result<u64, ModelError> {
    let mut value = 0;
    for byte in 0..u32::from(count) {
        let snapshot = snapshot(controller, vcpu, lane(first_interrupt, byte)?)?;
        value |= u64::from(snapshot.priority) << (byte * 8);
    }
    Ok(value)
}

fn write_priority(
    controller: &mut VirtualGic,
    vcpu: VirtualCpuId,
    first_interrupt: u32,
    count: u8,
    value: u64,
) -> Result<(), ModelError> {
    for byte in 0..u32::from(count) {
        let _ = snapshot(controller, vcpu, lane(first_interrupt, byte)?)?;
    }
    for byte in 0..u32::from(count) {
        let id = lane(first_interrupt, byte)?;
        controller.set_priority(
            interrupt(id)?,
            target_for(controller, id, vcpu)?,
            (value >> (byte * 8)) as u8,
        )?;
    }
    Ok(())
}

fn read_configuration(
    controller: &VirtualGic,
    vcpu: VirtualCpuId,
    first_interrupt: u32,
) -> Result<u64, ModelError> {
    let mut value = 0;
    for field in 0..16u32 {
        let id = lane(first_interrupt, field)?;
        if snapshot(controller, vcpu, id)?.trigger == InterruptTrigger::Edge {
            value |= 2 << (field * 2);
        }
    }
    Ok(value)
}

fn write_configuration(
    controller: &mut VirtualGic,
    vcpu: VirtualCpuId,
    first_interrupt: u32,
    value: u64,
) -> Result<(), ModelError> {
    for field in 0..16u32 {
        let id = lane(first_interrupt, field)?;
        if id >= 16 {
            let _ = snapshot(controller, vcpu, id)?;
        }
    }
    for field in 0..16u32 {
        let id = lane(first_interrupt, field)?;
        // SGI trigger configuration is architecturally fixed and write-ignored.
        if id < 16 {
            continue;
        }
        let trigger = if value & (2 << (field * 2)) != 0 {
            InterruptTrigger::Edge
        } else {
            InterruptTrigger::Level
        };
        controller.set_trigger(interrupt(id)?, target_for(controller, id, vcpu)?, trigger)?;
    }
    Ok(())
}

fn snapshot(
    controller: &VirtualGic,
    vcpu: VirtualCpuId,
    id: u32,
) -> Result<InterruptSnapshot, ModelError> {
    controller
        .snapshot(interrupt(id)?, target_for(controller, id, vcpu)?)
        .map_err(Into::into)
}

fn interrupt(id: u32) -> Result<GicInterruptId, ModelError> {
    GicInterruptId::new(id).ok_or(ModelError::InvalidDecodedSpan)
}

fn lane(first_interrupt: u32, offset: u32) -> Result<u32, ModelError> {
    first_interrupt
        .checked_add(offset)
        .ok_or(ModelError::InvalidDecodedSpan)
}

fn target_for(
    controller: &VirtualGic,
    id: u32,
    private_cpu: VirtualCpuId,
) -> Result<VirtualCpuId, ModelError> {
    if id < 32 {
        Ok(private_cpu)
    } else {
        Ok(controller.target(interrupt(id)?)?)
    }
}
