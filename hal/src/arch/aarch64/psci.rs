// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::drivers::power::psci::{CallWidth, Conduit, Error, Psci};
use hyper::platform::{PsciConduit, PsciInfo};

#[derive(Clone, Copy)]
pub enum Aarch64PsciConduit {
    Smc,
    Hvc,
}

impl Conduit for Aarch64PsciConduit {
    const CALL_WIDTH: CallWidth = CallWidth::Bits64;

    fn invoke(self, function_id: u32, argument0: u64, argument1: u64, argument2: u64) -> u64 {
        let mut result = u64::from(function_id);
        // SAFETY: The conduit is selected from the firmware DT binding. PSCI
        // uses SMCCC register assignments and permits x4-x17 to be clobbered by
        // the higher-privilege firmware implementation.
        unsafe {
            match self {
                Self::Smc => asm!(
                    "smc #0",
                    inlateout("x0") result,
                    in("x1") argument0,
                    in("x2") argument1,
                    in("x3") argument2,
                    lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                    lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                    lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                    lateout("x16") _, lateout("x17") _,
                    options(nostack)
                ),
                Self::Hvc => asm!(
                    "hvc #0",
                    inlateout("x0") result,
                    in("x1") argument0,
                    in("x2") argument1,
                    in("x3") argument2,
                    lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                    lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                    lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                    lateout("x16") _, lateout("x17") _,
                    options(nostack)
                ),
            }
        }
        result
    }
}

pub type Aarch64Psci = Psci<Aarch64PsciConduit>;

pub fn bind(info: PsciInfo) -> Result<Aarch64Psci, Error> {
    let conduit = match info.conduit {
        PsciConduit::Smc => Aarch64PsciConduit::Smc,
        PsciConduit::Hvc => Aarch64PsciConduit::Hvc,
    };
    Psci::initialize(conduit, info.interface)
}
