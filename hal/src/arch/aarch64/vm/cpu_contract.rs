// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure construction contract for the guest-visible `AArch64` CPU model.
//!
//! Hardware collection and publication live in `guest_cpu_model`. Keeping the
//! filtering rules here makes compatibility-group admission host-testable.

use crate::arch::aarch64::registers;

/// Raw hardware values which can contribute to the virtual CPU identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawGuestCpuFeatures {
    pub midr: u64,
    pub revidr: u64,
    pub pfr0: u64,
    pub isar0: u64,
    pub isar1: u64,
    pub isar2: u64,
    pub mmfr0: u64,
    pub mmfr1: u64,
    pub mmfr2: u64,
    pub ctr: u64,
    pub dczid: u64,
    pub cntfrq: u64,
}

/// Immutable values exposed by every vCPU in one admitted CPU group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuestCpuModel {
    values: [u64; Self::VALUE_COUNT],
}

impl GuestCpuModel {
    pub(crate) const VALUE_COUNT: usize = 12;

    /// Builds the deliberately restricted guest model supported by the saved
    /// vCPU context. Returning `None` rejects hardware below that baseline.
    pub(crate) const fn from_raw(raw: RawGuestCpuFeatures) -> Option<Self> {
        if field(
            raw.pfr0,
            registers::ID_AA64PFR0_FP_SHIFT,
            registers::ID_AA64PFR0_FP_MASK,
        ) == registers::ID_AA64PFR0_FP_NONE
            || field(
                raw.pfr0,
                registers::ID_AA64PFR0_ADVSIMD_SHIFT,
                registers::ID_AA64PFR0_ADVSIMD_MASK,
            ) == registers::ID_AA64PFR0_ADVSIMD_NONE
        {
            return None;
        }
        Some(Self {
            values: [
                raw.midr,
                raw.revidr,
                registers::ID_AA64PFR0_GUEST_BASE,
                raw.isar0 & !registers::ID_AA64ISAR0_TME_MASK,
                raw.isar1 & !registers::ID_AA64ISAR1_POINTER_AUTH_MASK,
                0,
                raw.mmfr0,
                raw.mmfr1 & !registers::ID_AA64MMFR1_VH_FIELD_MASK,
                raw.mmfr2 & !registers::ID_AA64MMFR2_NV_MASK,
                raw.ctr,
                raw.dczid,
                raw.cntfrq,
            ],
        })
    }

    pub(crate) const fn from_values(values: [u64; Self::VALUE_COUNT]) -> Self {
        Self { values }
    }

    pub(crate) const fn values(self) -> [u64; Self::VALUE_COUNT] {
        self.values
    }

    pub(crate) const fn midr(self) -> u64 {
        self.values[0]
    }

    pub(crate) const fn revidr(self) -> u64 {
        self.values[1]
    }

    pub(crate) const fn pfr0(self) -> u64 {
        self.values[2]
    }

    pub(crate) const fn isar0(self) -> u64 {
        self.values[3]
    }

    pub(crate) const fn isar1(self) -> u64 {
        self.values[4]
    }

    pub(crate) const fn isar2(self) -> u64 {
        self.values[5]
    }

    pub(crate) const fn mmfr0(self) -> u64 {
        self.values[6]
    }

    pub(crate) const fn mmfr1(self) -> u64 {
        self.values[7]
    }

    pub(crate) const fn mmfr2(self) -> u64 {
        self.values[8]
    }

    pub(crate) const fn ctr(self) -> u64 {
        self.values[9]
    }

    pub(crate) const fn dczid(self) -> u64 {
        self.values[10]
    }

    pub(crate) const fn cntfrq(self) -> u64 {
        self.values[11]
    }
}

const fn field(value: u64, shift: u64, mask: u64) -> u64 {
    (value >> shift) & mask
}
