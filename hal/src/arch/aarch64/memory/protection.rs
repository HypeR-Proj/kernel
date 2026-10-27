// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Local `AArch64` execution-permission controls.

use core::arch::asm;

use crate::arch::aarch64::registers;

/// Enables hardware enforcement of writable-implies-execute-never locally.
///
/// Final stage-1 descriptors are already split into RX, RO/XN and RW/XN
/// regions. WXN provides a second hardware backstop against an accidentally
/// writable executable mapping. The TLB flush is required because WXN is
/// permitted to be cached in translation entries.
pub fn enable_local_memory_protection() {
    // SAFETY: SCTLR_EL2 and the EL2 TLB are private to the current processing
    // element. Callers execute from a read-only high kernel mapping.
    unsafe {
        asm!(
            "mrs {control}, SCTLR_EL2",
            "orr {control}, {control}, {wxn}",
            "dsb ishst",
            "msr SCTLR_EL2, {control}",
            "isb",
            "tlbi alle2is",
            "dsb ish",
            "isb",
            control = out(reg) _,
            wxn = in(reg) registers::SCTLR_WXN,
            options(nostack, preserves_flags)
        );
    }
}

pub fn local_memory_protection_enabled() -> bool {
    let control: u64;
    // SAFETY: Reading local SCTLR_EL2 has no side effects.
    unsafe {
        asm!(
            "mrs {control}, SCTLR_EL2",
            control = out(reg) control,
            options(nomem, nostack, preserves_flags)
        );
    }
    control & registers::SCTLR_WXN != 0
}
