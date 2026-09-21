// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};

pub struct Riscv64Barrier;

impl Barrier for Riscv64Barrier {
    fn data_memory(_domain: BarrierDomain, access: BarrierAccess) {
        fence(access);
    }

    fn data_synchronization(_domain: BarrierDomain, access: BarrierAccess) {
        fence(access);
    }

    fn instruction_synchronization() {
        // SAFETY: FENCE.I is part of the selected Zifencei execution contract.
        unsafe { asm!("fence.i", options(nostack)) };
    }
}

fn fence(access: BarrierAccess) {
    // RISC-V FENCE has no Arm-style shareability domains. Include both memory
    // and device-I/O sets: platform MMIO may be classified as I/O rather than
    // ordinary reads and writes by the execution environment.
    match access {
        // A read barrier orders prior memory reads and device inputs before
        // every later access, matching the HAL's load-barrier contract.
        // SAFETY: FENCE has no pointer operands and is valid in HS mode.
        BarrierAccess::Reads => unsafe { asm!("fence ir, iorw", options(nostack)) },
        // A write barrier orders prior memory writes and device outputs before
        // later writes or outputs without unnecessarily constraining reads.
        // SAFETY: FENCE has no pointer operands and is valid in HS mode.
        BarrierAccess::Writes => unsafe { asm!("fence ow, ow", options(nostack)) },
        // SAFETY: FENCE has no pointer operands and is valid in HS mode.
        BarrierAccess::All => unsafe { asm!("fence iorw, iorw", options(nostack)) },
    }
}
