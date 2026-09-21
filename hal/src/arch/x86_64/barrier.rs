// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::{asm, x86_64::__cpuid};
use core::sync::atomic::{Ordering, compiler_fence};

use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};

pub struct X86_64Barrier;

impl Barrier for X86_64Barrier {
    fn data_memory(_domain: BarrierDomain, access: BarrierAccess) {
        match access {
            // SAFETY: Fence instructions have no pointer operands and are valid at CPL0.
            BarrierAccess::Reads => unsafe { asm!("lfence", options(nostack)) },
            // SAFETY: Fence instructions have no pointer operands and are valid at CPL0.
            BarrierAccess::Writes => unsafe { asm!("sfence", options(nostack)) },
            // SAFETY: Fence instructions have no pointer operands and are valid at CPL0.
            BarrierAccess::All => unsafe { asm!("mfence", options(nostack)) },
        }
    }

    fn data_synchronization(_domain: BarrierDomain, access: BarrierAccess) {
        Self::data_memory(BarrierDomain::FullSystem, access);
    }

    fn instruction_synchronization() {
        compiler_fence(Ordering::SeqCst);
        // CPUID is serializing and the intrinsic preserves LLVM's reserved RBX.
        let _ = __cpuid(0);
        compiler_fence(Ordering::SeqCst);
    }
}
