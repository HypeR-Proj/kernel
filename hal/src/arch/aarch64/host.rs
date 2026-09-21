// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Validation of the required VHE EL2 host execution regime.

use super::registers;
use core::arch::asm;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitializationError {
    VheRequired,
}

pub(super) fn initialize() -> Result<(), InitializationError> {
    if current_cpu_is_compatible() {
        Ok(())
    } else {
        Err(InitializationError::VheRequired)
    }
}

pub(super) fn current_cpu_is_compatible() -> bool {
    let features: u64;
    let hcr: u64;
    // SAFETY: These baseline EL2-readable registers have no side effects.
    // Bootstrap admits VHE before any VHE-only register alias is accessed.
    unsafe {
        asm!("mrs {features}, ID_AA64MMFR1_EL1", "mrs {hcr}, HCR_EL2",
            features = out(reg) features, hcr = out(reg) hcr,
            options(nomem, nostack, preserves_flags));
    }
    (features >> registers::ID_AA64MMFR1_VH_SHIFT) & registers::ID_AA64MMFR1_VH_MASK
        == registers::ID_AA64MMFR1_VH_VHE
        && hcr & (registers::HCR_EL2_E2H | registers::HCR_EL2_TGE)
            == registers::HCR_EL2_E2H | registers::HCR_EL2_TGE
}

pub const fn mode_name() -> &'static str {
    "VHE"
}
