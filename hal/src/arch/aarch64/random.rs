// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional boot-time seed. Firmware remains responsible for a TRNG's physical
//! quality; architectural RNDR is a fallback, not a mandatory CPU feature.

use core::arch::asm;
use {hyper_platform::CpuPowerInfo, hyper_platform::PsciConduit, hyper_platform::PsciInterface};

pub(crate) fn seed(power: Option<CpuPowerInfo>) -> Option<u64> {
    // HypeR runs at EL2. HVC is not a valid conduit to EL3 from here; do not
    // guess SMC availability on platforms that only advertise HVC/legacy PSCI.
    if matches!(power, Some(CpuPowerInfo::Psci(info))
        if info.conduit == PsciConduit::Smc && matches!(info.interface, PsciInterface::Standard(_)))
        && let Some(seed) = hyper_drivers::random::trng::seed(smc)
    {
        return Some(seed);
    }
    let features: u64;
    // SAFETY: ID_AA64ISAR0_EL1 is readable at the required host EL2.
    unsafe {
        asm!("mrs {features}, ID_AA64ISAR0_EL1", features = out(reg) features, options(nomem, nostack, preserves_flags))
    };
    if features >> 60 != 1 {
        return None;
    }
    let value: u64;
    let success: u32;
    // SAFETY: FEAT_RNG was checked. RNDR reports availability in NZCV; do not
    // claim preserves_flags or consume the returned bits when Z is set.
    unsafe {
        asm!("mrs {value}, S3_3_C2_C4_0", "cset {success:w}, ne",
            value = out(reg) value, success = out(reg) success, options(nomem, nostack));
    }
    (success != 0).then_some(value)
}

fn smc(function: u32, argument: u64) -> [u64; 4] {
    let mut x0 = u64::from(function);
    let mut x1 = argument;
    let x2: u64;
    let x3: u64;
    // SAFETY: Called only after discovery of the standard SMC firmware
    // conduit. Describe all SMCCC scratch registers, including x1-x3 outputs.
    unsafe {
        asm!("smc #0", inlateout("x0") x0, inlateout("x1") x1,
            inlateout("x2") 0u64 => x2, inlateout("x3") 0u64 => x3,
            lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
            lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
            lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
            lateout("x16") _, lateout("x17") _, options(nostack));
    }
    [x0, x1, x2, x3]
}
