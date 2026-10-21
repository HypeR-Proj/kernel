// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `PMUv3` cycle-counter sampling at EL2. The host exclusively owns the PMU.

use super::registers::*;
use core::arch::asm;

pub fn available() -> bool {
    let id: u64;
    // SAFETY: The architectural feature register is read-only at EL2.
    unsafe {
        asm!("mrs {id}, id_aa64dfr0_el1", id = out(reg) id, options(nomem, nostack, preserves_flags))
    };
    matches!((id >> 8) & 15, 1..=8)
}

/// Takes exclusive ownership after feature detection, with local IRQs masked.
/// Clear inherited event-counter state too: the profiler only uses PMCCNTR.
pub fn initialize() {
    // SAFETY: Feature detection succeeded and the host exclusively owns
    // this CPU's PMU. Lower exception levels cannot program its registers.
    unsafe {
        asm!("msr pmintenclr_el1, {all}", "msr pmcntenclr_el0, {all}",
                  "msr pmovsclr_el0, {all}", "msr pmuserenr_el0, xzr", "isb",
                  all = in(reg) 0xffff_ffffu64, options(nostack, preserves_flags))
    };
}

/// Configures the calling CPU with IRQs masked and its PMU interrupt installed.
/// The cycle counter uses 64-bit overflow; other counters remain disabled.
pub fn start(period: u64) {
    stop();
    // Count cycles in all non-secure worlds, including EL2 when TGE=0.
    // VHE host EL2 uses P=0; Native/guest EL0 and guest EL1 remain enabled.
    let filter = PMCCFILTR_NSH;
    let mdcr: u64;
    // SAFETY: Masked local control and sampling exclusively own PMU state. PMUSERENR
    // denies direct Native access; TPM/TPMCR trap lower-EL PMU programming.
    unsafe {
        asm!(
            "mrs {mdcr}, mdcr_el2",
            "msr pmuserenr_el0, xzr",
            "msr pmccfiltr_el0, {filter}",
            "msr pmcr_el0, {control}",
            mdcr = out(reg) mdcr, filter = in(reg) filter,
            control = in(reg) (PMCR_E | PMCR_LC), options(nostack, preserves_flags)
        );
        asm!("msr mdcr_el2, {value}", "isb", value = in(reg) ((mdcr & !MDCR_EL2_HPMD) | MDCR_EL2_TPM | MDCR_EL2_TPMCR), options(nostack, preserves_flags));
    }
    reload(period);
}

pub fn overflowed() -> bool {
    let status: u64;
    // SAFETY: Called only after PMUv3 feature detection on this CPU.
    unsafe {
        asm!("mrs {status}, pmovsclr_el0", status = out(reg) status, options(nomem, nostack, preserves_flags))
    };
    status & PMU_CYCLE_BIT != 0
}

pub fn reload(period: u64) {
    // SAFETY: IRQs are masked; no other context programs this CPU's counter.
    unsafe {
        asm!("msr pmcntenclr_el0, {mask}", "isb",
             "msr pmccntr_el0, {initial}", "msr pmovsclr_el0, {mask}",
             "msr pmcntenset_el0, {mask}", "msr pmintenset_el1, {mask}", "isb",
             mask = in(reg) PMU_CYCLE_BIT, initial = in(reg) 0u64.wrapping_sub(period),
             options(nostack, preserves_flags));
    }
}

pub fn stop() {
    // SAFETY: Called only on a PMUv3 CPU, with local IRQs masked.
    unsafe {
        asm!("msr pmintenclr_el1, {mask}", "msr pmcntenclr_el0, {mask}", "msr pmovsclr_el0, {mask}", "isb", mask = in(reg) PMU_CYCLE_BIT, options(nostack, preserves_flags))
    };
}
