// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};

use hyper::hal::cpu_power::{
    CpuAffinityState, CpuHardwareId, CpuPower, CpuPowerCapabilities, CpuPowerVersion,
    ResumeAddress, SuspendState,
};
use hyper::hal::memory::AddressTranslation;
use hyper::mm::PhysicalAddress;
use hyper::platform::CpuPowerInfo;

const TRAMPOLINE: u64 = 0x8000;
const MAILBOX_BASE: u64 = 0x9000;
const MAILBOX_STRIDE: u64 = 64;
const X2APIC_ICR: u32 = 0x830;
const ICR_DELIVERY_STATUS: u64 = 1 << 12;
const ICR_INIT: u64 = 5 << 8;
const ICR_STARTUP: u64 = 6 << 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAddress,
    InvalidTarget,
    NotSupported,
    TrampolineTooLarge,
}

#[derive(Clone, Copy)]
pub struct X2ApicCpuPower {
    tsc_frequency: u64,
}

pub fn bind(info: CpuPowerInfo) -> Result<X2ApicCpuPower, Error> {
    match info {
        CpuPowerInfo::X86Apic(info) => {
            install_trampoline()?;
            Ok(X2ApicCpuPower {
                tsc_frequency: info.tsc_frequency,
            })
        }
        _ => Err(Error::NotSupported),
    }
}

impl CpuPower for X2ApicCpuPower {
    type Error = Error;

    fn capabilities(&self) -> CpuPowerCapabilities {
        CpuPowerCapabilities {
            version: CpuPowerVersion { major: 1, minor: 0 },
            cpu_suspend: false,
            cpu_off: false,
            cpu_on: true,
            affinity_info: false,
            system_off: false,
            system_reset: false,
        }
    }

    unsafe fn cpu_on(
        &self,
        target: CpuHardwareId,
        entry: ResumeAddress,
        context: u64,
    ) -> Result<(), Self::Error> {
        if entry.get() != TRAMPOLINE {
            return Err(Error::InvalidAddress);
        }
        let apic_id = u32::try_from(target.get()).map_err(|_| Error::InvalidTarget)?;
        let source = linear_pointer(context)? as *const u64;
        let mailbox_physical = MAILBOX_BASE
            .checked_add(u64::from(apic_id) * MAILBOX_STRIDE)
            .ok_or(Error::InvalidAddress)?;
        let mailbox = linear_pointer(mailbox_physical)? as *mut u64;
        for index in 0..5 {
            // SAFETY: CPU_ON retains the source record, and the reserved target
            // mailbox contains five writable words.
            unsafe { write_volatile(mailbox.add(index), read_volatile(source.add(index))) };
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        send_ipi(apic_id, ICR_INIT)?;
        self.delay(self.tsc_frequency / 100);
        send_ipi(apic_id, ICR_STARTUP | (TRAMPOLINE >> 12))?;
        self.delay(self.tsc_frequency / 5000);
        send_ipi(apic_id, ICR_STARTUP | (TRAMPOLINE >> 12))
    }

    fn cpu_off(&self) -> Result<(), Self::Error> {
        Err(Error::NotSupported)
    }

    unsafe fn cpu_suspend(
        &self,
        _state: SuspendState,
        _entry: ResumeAddress,
        _context: u64,
    ) -> Result<(), Self::Error> {
        Err(Error::NotSupported)
    }

    fn affinity_info(
        &self,
        _target: CpuHardwareId,
        _lowest_affinity_level: u8,
    ) -> Result<CpuAffinityState, Self::Error> {
        Err(Error::NotSupported)
    }

    fn system_off(&self) -> Result<(), Self::Error> {
        Err(Error::NotSupported)
    }

    fn system_reset(&self) -> Result<(), Self::Error> {
        Err(Error::NotSupported)
    }
}

impl X2ApicCpuPower {
    fn delay(self, ticks: u64) {
        let start = rdtsc();
        while rdtsc().wrapping_sub(start) < ticks {
            core::hint::spin_loop();
        }
    }
}

fn install_trampoline() -> Result<(), Error> {
    unsafe extern "C" {
        static x86_64_ap_trampoline_start: u8;
        static x86_64_ap_trampoline_end: u8;
    }
    let start = core::ptr::addr_of!(x86_64_ap_trampoline_start) as usize;
    let end = core::ptr::addr_of!(x86_64_ap_trampoline_end) as usize;
    let length = end.checked_sub(start).ok_or(Error::InvalidAddress)?;
    if length > 4096 {
        return Err(Error::TrampolineTooLarge);
    }
    let destination = linear_pointer(TRAMPOLINE)? as *mut u8;
    // SAFETY: Linker symbols bound a live source; the reserved trampoline page
    // is non-overlapping, writable, and large enough.
    unsafe { copy_nonoverlapping(start as *const u8, destination, length) };
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    Ok(())
}

fn linear_pointer(physical: u64) -> Result<usize, Error> {
    super::memory::X86_64AddressTranslation::linear_address(PhysicalAddress::new(physical))
        .and_then(|address| usize::try_from(address.get()).ok())
        .ok_or(Error::InvalidAddress)
}

fn send_ipi(target: u32, command: u64) -> Result<(), Error> {
    let value = (u64::from(target) << 32) | command;
    // SAFETY: The bound platform exposes the x2APIC ICR MSR at CPL0.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") X2APIC_ICR,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        )
    };
    while read_icr() & ICR_DELIVERY_STATUS != 0 {
        core::hint::spin_loop();
    }
    Ok(())
}

fn read_icr() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: The bound platform exposes the x2APIC ICR MSR at CPL0.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") X2APIC_ICR,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        )
    };
    (u64::from(high) << 32) | u64::from(low)
}

fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC has no pointer operands and is supported by the target profile.
    unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack)) };
    (u64::from(high) << 32) | u64::from(low)
}
