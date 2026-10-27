// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Privileged Intel VMX instruction boundary.
//!
//! Callers own VMX lifecycle and VMCS field policy. This module only binds
//! Rust values to architectural operands, captures CF/ZF instruction status,
//! and contains the raw CPL0 assembly required by that contract.

use core::arch::asm;

use crate::arch::x86_64::vm::vmx::Error;

#[repr(C, align(16))]
struct InveptDescriptor {
    eptp: u64,
    reserved: u64,
}

pub(super) fn invept_single_context(eptp: u64) -> Result<(), Error> {
    let descriptor = InveptDescriptor { eptp, reserved: 0 };
    let failed: u8;
    // SAFETY: The caller established VMX operation. `descriptor` is a live,
    // aligned single-context INVEPT operand for the supplied EPTP.
    unsafe {
        asm!(
            "invept {kind}, [{descriptor}]",
            "setna {failed}",
            kind = in(reg) 1_u64,
            descriptor = in(reg) &descriptor,
            failed = out(reg_byte) failed,
            options(nostack)
        )
    };
    status(failed)
}

pub(super) fn vmxon(address: u64) -> Result<(), Error> {
    let failed: u8;
    // SAFETY: The caller validated and revision-initialized this pinned VMXON
    // region and installed Intel's fixed CR requirements.
    unsafe {
        asm!("vmxon [{}]", "setna {}", in(reg) &address, out(reg_byte) failed, options(nostack))
    };
    status(failed)
}

pub(super) fn vmclear(address: u64) -> Result<(), Error> {
    let failed: u8;
    // SAFETY: The caller supplies a pinned VMCS region while VMX is active.
    unsafe {
        asm!("vmclear [{}]", "setna {}", in(reg) &address, out(reg_byte) failed, options(nostack))
    };
    status(failed)
}

pub(super) fn vmptrld(address: u64) -> Result<(), Error> {
    let failed: u8;
    // SAFETY: The caller supplies a validated, cleared VMCS owned by this CPU.
    unsafe {
        asm!("vmptrld [{}]", "setna {}", in(reg) &address, out(reg_byte) failed, options(nostack))
    };
    status(failed)
}

pub(super) fn vmwrite(field: u64, value: u64) -> Result<(), Error> {
    let failed: u8;
    // SAFETY: The caller owns the current VMCS and supplies an admitted field
    // encoding/value. VMX status flags are captured before returning.
    unsafe {
        asm!("vmwrite {field}, {value}", "setna {failed}", value = in(reg) value, field = in(reg) field, failed = out(reg_byte) failed, options(nostack))
    };
    status(failed)
}

pub(super) fn vmread(field: u64) -> Result<u64, Error> {
    let value: u64;
    let failed: u8;
    // SAFETY: The caller owns the current VMCS and supplies an admitted field
    // encoding. The value is exposed only when VMX reports success.
    unsafe {
        asm!("vmread {value}, {field}", "setna {failed}", field = in(reg) field, value = out(reg) value, failed = out(reg_byte) failed, options(nostack))
    };
    if failed == 0 {
        Ok(value)
    } else {
        Err(Error::VmxInstruction)
    }
}

const fn status(failed: u8) -> Result<(), Error> {
    if failed == 0 {
        Ok(())
    } else {
        Err(Error::VmxInstruction)
    }
}

pub(super) fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: VMX callers execute at CPL0 and select VMX/host-state MSRs.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack))
    };
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes a model-specific register at CPL0.
///
/// # Safety
///
/// `msr` must be writable on this processor and `value` must preserve every
/// architectural invariant on which the running Rust kernel depends.
pub(super) unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: The caller supplies the admitted MSR/value pair.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nostack))
    };
}

macro_rules! read_register {
    ($name:ident, $instruction:literal, $type:ty) => {
        pub(super) fn $name() -> $type {
            let value: $type;
            // SAFETY: Invocations read only current-CPU control, stack,
            // segment, or task registers while executing at CPL0.
            unsafe { asm!($instruction, out(reg) value, options(nomem, nostack)) };
            value
        }
    };
}

read_register!(read_cr0, "mov {}, cr0", u64);
read_register!(read_cr3, "mov {}, cr3", u64);
read_register!(read_cr4, "mov {}, cr4", u64);
read_register!(read_rsp, "mov {}, rsp", u64);
read_register!(read_cs, "mov {:x}, cs", u16);
read_register!(read_ss, "mov {:x}, ss", u16);
read_register!(read_ds, "mov {:x}, ds", u16);
read_register!(read_es, "mov {:x}, es", u16);
read_register!(read_fs, "mov {:x}, fs", u16);
read_register!(read_gs, "mov {:x}, gs", u16);
read_register!(read_tr, "str {:x}", u16);

/// # Safety
///
/// `value` must preserve paging, protection, and all Intel fixed-bit
/// requirements needed by the executing Rust kernel.
pub(super) unsafe fn write_cr0(value: u64) {
    // SAFETY: The caller establishes the control-register contract.
    unsafe { asm!("mov cr0, {}", in(reg) value, options(nostack)) };
}

/// # Safety
///
/// `value` must preserve the host execution features and Intel fixed bits and
/// may enable VMXE only after VMX capability validation.
pub(super) unsafe fn write_cr4(value: u64) {
    // SAFETY: The caller establishes the control-register contract.
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nostack)) };
}
