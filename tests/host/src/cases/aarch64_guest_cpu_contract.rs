// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host tests for the frozen `AArch64` guest CPU compatibility group.

use crate::aarch64_guest_cpu_contract_model::{GuestCpuModel, RawGuestCpuFeatures};
use crate::registers;

fn baseline() -> RawGuestCpuFeatures {
    RawGuestCpuFeatures {
        midr: 0x410f_d0c0,
        revidr: 1,
        pfr0: registers::ID_AA64PFR0_GUEST_BASE,
        isar0: 0x1111_1111_1111_1111,
        isar1: 0x2222_2222_2222_2222,
        isar2: 0x3333_3333_3333_3333,
        mmfr0: 0x4444_4444_4444_4444,
        mmfr1: 0x5555_5555_5555_5555,
        mmfr2: 0x6666_6666_6666_6666,
        ctr: 0x8444_c004,
        dczid: 4,
        cntfrq: 62_500_000,
    }
}

fn model(raw: RawGuestCpuFeatures) -> GuestCpuModel {
    GuestCpuModel::from_raw(raw).unwrap_or_else(|| panic!("valid guest CPU baseline was rejected"))
}

#[test]
fn hidden_hardware_features_do_not_split_a_compatibility_group() {
    let boot = baseline();
    let mut secondary = boot;
    secondary.pfr0 ^= 1 << 32;
    secondary.isar0 ^= registers::ID_AA64ISAR0_TME_MASK;
    secondary.isar1 ^= registers::ID_AA64ISAR1_POINTER_AUTH_MASK;
    secondary.isar2 = 0;
    secondary.mmfr1 ^= registers::ID_AA64MMFR1_VH_FIELD_MASK;
    secondary.mmfr2 ^= registers::ID_AA64MMFR2_NV_MASK;

    assert_eq!(model(boot), model(secondary));
}

#[test]
fn every_guest_visible_difference_splits_the_compatibility_group() {
    let boot = baseline();
    let frozen = model(boot);
    macro_rules! reject_change {
        ($field:ident, $value:expr) => {{
            let mut secondary = boot;
            secondary.$field = $value;
            assert_ne!(frozen, model(secondary), stringify!($field));
        }};
    }

    reject_change!(midr, boot.midr ^ 1);
    reject_change!(revidr, boot.revidr ^ 1);
    reject_change!(isar0, boot.isar0 ^ 1);
    reject_change!(isar1, boot.isar1 ^ 1);
    reject_change!(mmfr0, boot.mmfr0 ^ 1);
    reject_change!(mmfr1, boot.mmfr1 ^ 1);
    reject_change!(mmfr2, boot.mmfr2 ^ 1);
    reject_change!(ctr, boot.ctr ^ 1);
    reject_change!(dczid, boot.dczid ^ 1);
    reject_change!(cntfrq, boot.cntfrq + 1);
}

#[test]
fn model_rejects_hardware_without_saved_simd_state_support() {
    let mut raw = baseline();
    raw.pfr0 |= registers::ID_AA64PFR0_FP_NONE << registers::ID_AA64PFR0_FP_SHIFT;
    assert!(GuestCpuModel::from_raw(raw).is_none());

    raw = baseline();
    raw.pfr0 |= registers::ID_AA64PFR0_ADVSIMD_NONE << registers::ID_AA64PFR0_ADVSIMD_SHIFT;
    assert!(GuestCpuModel::from_raw(raw).is_none());
}

#[test]
fn frozen_model_reports_only_the_supported_virtual_contract() {
    let raw = baseline();
    let model = model(raw);

    assert_eq!(model.midr(), raw.midr);
    assert_eq!(model.revidr(), raw.revidr);
    assert_eq!(model.pfr0(), registers::ID_AA64PFR0_GUEST_BASE);
    assert_eq!(model.isar0() & registers::ID_AA64ISAR0_TME_MASK, 0);
    assert_eq!(model.isar1() & registers::ID_AA64ISAR1_POINTER_AUTH_MASK, 0);
    assert_eq!(model.isar2(), 0);
    assert_eq!(model.mmfr0(), raw.mmfr0);
    assert_eq!(model.mmfr1() & registers::ID_AA64MMFR1_VH_FIELD_MASK, 0);
    assert_eq!(model.mmfr2() & registers::ID_AA64MMFR2_NV_MASK, 0);
    assert_eq!(model.ctr(), raw.ctr);
    assert_eq!(model.dczid(), raw.dczid);
    assert_eq!(model.cntfrq(), raw.cntfrq);
    assert_eq!(GuestCpuModel::from_values(model.values()), model);
}
