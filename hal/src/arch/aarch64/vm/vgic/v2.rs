// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Banked GICH registers and the guest-only GICV window.
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use hyper::sync::PublishedOnce;
use hyper_platform::GicV2Info;
use hyper_vm_models::arm::gic::lr_v2;
use {
    crate::arch::aarch64::vm::vgic::Capabilities, crate::arch::aarch64::vm::vgic::CpuContext,
    crate::arch::aarch64::vm::vgic::Error, crate::arch::aarch64::vm::vgic::MaintenanceState,
};

struct Interface {
    base: usize,
    guest_physical: u64,
}
static INTERFACE: PublishedOnce<Interface> = PublishedOnce::new();

/// # Safety
/// The caller owns permanent Device mappings and has local IRQs masked. GICH
/// accesses below are banked by the executing CPU; callers serialize each bank.
pub unsafe fn install(
    info: GicV2Info,
    map: &mut impl FnMut(u64) -> Option<usize>,
) -> Result<(), Error> {
    let (Some(control), Some(guest), Some(_)) = (
        info.hypervisor_interface,
        info.virtual_cpu_interface,
        info.maintenance_interrupt,
    ) else {
        return Ok(());
    };
    if control.size() < 0x1000
        || !control.start().is_multiple_of(0x1000)
        || guest.size() < 0x2000
        || !guest.start().is_multiple_of(0x1000)
    {
        return Err(Error::InvalidTypeRegister);
    }
    let base = map(control.start()).ok_or(Error::InvalidTypeRegister)?;
    if !base.is_multiple_of(4) || base.checked_add(0xfff).is_none() {
        return Err(Error::InvalidTypeRegister);
    }
    // SAFETY: The validated, permanently mapped GICH page belongs to this
    // boot transaction. Probe before publishing the selected backend.
    let vtr = unsafe { read_volatile((base + 4) as *const u32) };
    decode_capabilities(vtr)?;
    INTERFACE
        .publish(Interface {
            base,
            guest_physical: guest.start(),
        })
        .map_err(|_| Error::StateMismatch)?;
    disable();
    Ok(())
}
pub fn guest_physical() -> Option<u64> {
    INTERFACE.get().map(|i| i.guest_physical)
}
fn read(offset: usize) -> u32 {
    let Some(interface) = INTERFACE.get() else {
        return 0;
    };
    // SAFETY: Installation validated the permanent banked GICH page. Every
    // caller supplies a word-aligned constant or an implemented LR index.
    unsafe { read_volatile((interface.base + offset) as *const u32) }
}
fn write(offset: usize, value: u32) {
    let Some(interface) = INTERFACE.get() else {
        return;
    };
    // SAFETY: Same permanent Device mapping and local-bank ownership as read.
    unsafe { write_volatile((interface.base + offset) as *mut u32, value) }
}
fn synchronize() {
    // SAFETY: Complete MMIO state writes before guest execution or bank reuse.
    unsafe {
        asm!("dsb sy", "isb", options(nostack, preserves_flags));
    }
}
pub fn capabilities() -> Result<Capabilities, Error> {
    if INTERFACE.get().is_none() {
        return Err(Error::IncompatibleCpuInterface);
    }
    decode_capabilities(read(4))
}
fn decode_capabilities(vtr: u32) -> Result<Capabilities, Error> {
    let list_registers = ((vtr & 63) + 1) as u8;
    let priority_bits = (((vtr >> 29) & 7) + 1) as u8;
    let preemption_bits = (((vtr >> 26) & 7) + 1) as u8;
    // GICv2 LRs carry five priority bits and GICH_APR has 32 active levels.
    if priority_bits != 5 || preemption_bits != 5 {
        return Err(Error::InvalidTypeRegister);
    }
    Ok(Capabilities {
        list_registers,
        priority_bits,
        preemption_bits,
        interrupt_id_bits: 10,
    })
}
pub fn disable() {
    write(0, 0);
    synchronize();
}
fn validate(context: &CpuContext) -> Result<(), Error> {
    if capabilities()?.list_registers != context.list_register_count {
        return Err(Error::IncompatibleCpuInterface);
    }
    Ok(())
}
pub(super) fn activate(context: &CpuContext) -> Result<(), Error> {
    validate(context)?;
    disable();
    write(8, context.virtual_machine_control as u32);
    write(0xf0, context.active_priorities_group0[0] as u32);
    for (index, entry) in context.slots().iter().enumerate() {
        write(0x100 + index * 4, lr_v2::encode(*entry));
    }
    synchronize();
    write(0, context.control as u32 | 1);
    synchronize();
    Ok(())
}
pub(super) fn deactivate(context: &mut CpuContext) -> Result<(), Error> {
    validate(context)?;
    context.control = u64::from(read(0));
    disable();
    context.virtual_machine_control = u64::from(read(8));
    context.active_priorities_group0[0] = u64::from(read(0xf0));
    for (index, entry) in context.slots_mut().iter_mut().enumerate() {
        *entry =
            lr_v2::decode(read(0x100 + index * 4)).map_err(|_| Error::InvalidVirtualInterrupt)?;
    }
    Ok(())
}
pub fn maintenance_state() -> MaintenanceState {
    MaintenanceState {
        status: u64::from(read(0x10)),
        eoi_list_registers: u64::from(read(0x20)) | (u64::from(read(0x24)) << 32),
        empty_list_registers: u64::from(read(0x30)) | (u64::from(read(0x34)) << 32),
    }
}
