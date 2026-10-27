// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Frozen guest CPU identity and physical-CPU compatibility admission.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use {
    crate::arch::aarch64::vm::cpu_contract::GuestCpuModel,
    crate::arch::aarch64::vm::cpu_contract::RawGuestCpuFeatures,
};

const UNINITIALIZED: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNINITIALIZED);
static VALUES: [AtomicU64; GuestCpuModel::VALUE_COUNT] =
    [const { AtomicU64::new(0) }; GuestCpuModel::VALUE_COUNT];

/// Freezes the boot CPU's sanitized model before any secondary is released.
pub(crate) fn initialize_boot_cpu() -> bool {
    if STATE
        .compare_exchange(
            UNINITIALIZED,
            INITIALIZING,
            Ordering::Acquire,
            Ordering::Acquire,
        )
        .is_err()
    {
        return false;
    }
    let Some(model) = GuestCpuModel::from_raw(read_current_features()) else {
        return false;
    };
    for (slot, value) in VALUES.iter().zip(model.values()) {
        slot.store(value, Ordering::Relaxed);
    }
    STATE.store(READY, Ordering::Release);
    true
}

/// Rejects a PE whose sanitized guest-visible model differs from the frozen
/// model. Differences in deliberately hidden features do not reject a PE.
pub(crate) fn current_cpu_is_compatible() -> bool {
    let Some(current) = GuestCpuModel::from_raw(read_current_features()) else {
        return false;
    };
    load_frozen().is_some_and(|frozen| frozen == current)
}

/// Returns the immutable model used by trapped guest ID-register reads.
pub(crate) fn frozen() -> GuestCpuModel {
    match load_frozen() {
        Some(model) => model,
        None => hyper::debug::invariant_failure("aarch64/guest_cpu_model::frozen invariant"),
    }
}

/// Returns the frozen processor identity without loading the remaining model.
pub(crate) fn processor_identity() -> u64 {
    if STATE.load(Ordering::Acquire) != READY {
        hyper::debug::invariant_failure("aarch64/guest_cpu_model::processor_identity invariant")
    }
    VALUES[0].load(Ordering::Relaxed)
}

fn load_frozen() -> Option<GuestCpuModel> {
    if STATE.load(Ordering::Acquire) != READY {
        return None;
    }
    let mut values = [0; GuestCpuModel::VALUE_COUNT];
    for (destination, source) in values.iter_mut().zip(VALUES.iter()) {
        *destination = source.load(Ordering::Relaxed);
    }
    Some(GuestCpuModel::from_values(values))
}

fn read_current_features() -> RawGuestCpuFeatures {
    RawGuestCpuFeatures {
        midr: read_midr_el1(),
        revidr: read_revidr_el1(),
        pfr0: read_id_aa64pfr0_el1(),
        isar0: read_id_aa64isar0_el1(),
        isar1: read_id_aa64isar1_el1(),
        isar2: read_id_aa64isar2_el1(),
        mmfr0: read_id_aa64mmfr0_el1(),
        mmfr1: read_id_aa64mmfr1_el1(),
        mmfr2: read_id_aa64mmfr2_el1(),
        ctr: read_ctr_el0(),
        dczid: read_dczid_el0(),
        cntfrq: read_cntfrq_el0(),
    }
}

macro_rules! read_register {
    ($function:ident, $register:literal) => {
        fn $function() -> u64 {
            let value: u64;
            // SAFETY: The named identification register is read-only at EL2.
            unsafe {
                asm!(
                    concat!("mrs {value}, ", $register),
                    value = out(reg) value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }
    };
}

read_register!(read_midr_el1, "MIDR_EL1");
read_register!(read_revidr_el1, "REVIDR_EL1");
read_register!(read_id_aa64pfr0_el1, "ID_AA64PFR0_EL1");
read_register!(read_id_aa64isar0_el1, "ID_AA64ISAR0_EL1");
read_register!(read_id_aa64isar1_el1, "ID_AA64ISAR1_EL1");
read_register!(read_id_aa64isar2_el1, "ID_AA64ISAR2_EL1");
read_register!(read_id_aa64mmfr0_el1, "ID_AA64MMFR0_EL1");
read_register!(read_id_aa64mmfr1_el1, "ID_AA64MMFR1_EL1");
read_register!(read_id_aa64mmfr2_el1, "ID_AA64MMFR2_EL1");
read_register!(read_ctr_el0, "CTR_EL0");
read_register!(read_dczid_el0, "DCZID_EL0");
read_register!(read_cntfrq_el0, "CNTFRQ_EL0");
