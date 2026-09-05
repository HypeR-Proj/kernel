// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! PSCI conduit selection and firmware-call ABI encoding.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use hyper::drivers::power::psci::{CallWidth, Conduit, Error, Psci};
use hyper::hal::cpu_power::{CpuHardwareId, CpuPower, ResumeAddress};
use hyper::platform::{PsciCompatibleVersion, PsciInterface, PsciLegacyFunctionIds};

const PSCI_VERSION: u32 = 0x8400_0000;
const PSCI_FEATURES: u32 = 0x8400_000a;
const PSCI_CPU_ON_32: u32 = 0x8400_0003;
const PSCI_CPU_ON_64: u32 = 0xc400_0003;

static LAST_FUNCTION: AtomicU32 = AtomicU32::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct Fake32;

#[derive(Clone, Copy)]
struct Fake64;

fn invoke(function_id: u32) -> u64 {
    LAST_FUNCTION.store(function_id, Ordering::Release);
    match function_id {
        PSCI_VERSION => 0x0001_0001,
        PSCI_FEATURES => 0,
        _ => 0,
    }
}

fn test_lock() -> MutexGuard<'static, ()> {
    match TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Conduit for Fake32 {
    const CALL_WIDTH: CallWidth = CallWidth::Bits32;

    fn invoke(self, function_id: u32, _argument0: u64, _argument1: u64, _argument2: u64) -> u64 {
        invoke(function_id)
    }
}

impl Conduit for Fake64 {
    const CALL_WIDTH: CallWidth = CallWidth::Bits64;

    fn invoke(self, function_id: u32, _argument0: u64, _argument1: u64, _argument2: u64) -> u64 {
        invoke(function_id)
    }
}

#[test]
fn selects_smccc32_function_ids_for_a_32_bit_conduit() {
    let _guard = test_lock();
    let controller = crate::require_ok(Psci::initialize(
        Fake32,
        PsciInterface::Standard(PsciCompatibleVersion::V1_0),
    ));
    // SAFETY: Fake32 records arguments and never starts a CPU or consumes
    // the synthetic entry/context addresses.
    crate::require_ok(unsafe {
        controller.cpu_on(CpuHardwareId::new(1), ResumeAddress::new(0x8000), 7)
    });
    assert_eq!(LAST_FUNCTION.load(Ordering::Acquire), PSCI_CPU_ON_32);
    assert_eq!(
        // SAFETY: The out-of-range entry is rejected before Fake32 can issue a
        // firmware call, so no resume trampoline is consumed.
        unsafe {
            controller.cpu_on(
                CpuHardwareId::new(1),
                ResumeAddress::new(u64::from(u32::MAX) + 1),
                0,
            )
        },
        Err(Error::InvalidAddress)
    );
}

#[test]
fn selects_smccc64_function_ids_for_a_64_bit_conduit() {
    let _guard = test_lock();
    let controller = crate::require_ok(Psci::initialize(
        Fake64,
        PsciInterface::Standard(PsciCompatibleVersion::V1_0),
    ));
    // SAFETY: Fake64 only records the arguments and does not execute the
    // synthetic resume address.
    crate::require_ok(unsafe {
        controller.cpu_on(CpuHardwareId::new(1), ResumeAddress::new(0x1_0000_8000), 7)
    });
    assert_eq!(LAST_FUNCTION.load(Ordering::Acquire), PSCI_CPU_ON_64);
}

#[test]
fn uses_legacy_dtb_function_ids_with_32_bit_arguments() {
    const LEGACY_CPU_OFF: u32 = 0x95c1_0001;
    const LEGACY_CPU_ON: u32 = 0x95c1_0002;

    let _guard = test_lock();
    let controller = crate::require_ok(Psci::initialize(
        Fake64,
        PsciInterface::Legacy(PsciLegacyFunctionIds {
            cpu_suspend: None,
            cpu_off: LEGACY_CPU_OFF,
            cpu_on: LEGACY_CPU_ON,
            migrate: None,
        }),
    ));
    assert_eq!(controller.capabilities().version.major, 0);
    assert_eq!(controller.capabilities().version.minor, 1);
    assert!(!controller.capabilities().affinity_info);
    // SAFETY: Fake64 records the legacy call without starting a CPU.
    crate::require_ok(unsafe {
        controller.cpu_on(CpuHardwareId::new(1), ResumeAddress::new(0x8000), 7)
    });
    assert_eq!(LAST_FUNCTION.load(Ordering::Acquire), LEGACY_CPU_ON);
    assert_eq!(
        // SAFETY: Legacy argument validation rejects this entry before Fake64
        // can issue a firmware call, so no resume trampoline is consumed.
        unsafe {
            controller.cpu_on(
                CpuHardwareId::new(1),
                ResumeAddress::new(u64::from(u32::MAX) + 1),
                0,
            )
        },
        Err(Error::InvalidAddress)
    );
}
