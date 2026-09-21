// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};

/// `AArch64` implementation of the architecture-neutral barrier contract.
pub struct Aarch64Barrier;

macro_rules! execute_barrier {
    ($instruction:literal, $domain:expr, $access:expr) => {
        match ($domain, $access) {
            (BarrierDomain::NonShareable, BarrierAccess::Reads) => {
                emit_barrier!($instruction, " nshld")
            }
            (BarrierDomain::NonShareable, BarrierAccess::Writes) => {
                emit_barrier!($instruction, " nshst")
            }
            (BarrierDomain::NonShareable, BarrierAccess::All) => {
                emit_barrier!($instruction, " nsh")
            }
            (BarrierDomain::InnerShareable, BarrierAccess::Reads) => {
                emit_barrier!($instruction, " ishld")
            }
            (BarrierDomain::InnerShareable, BarrierAccess::Writes) => {
                emit_barrier!($instruction, " ishst")
            }
            (BarrierDomain::InnerShareable, BarrierAccess::All) => {
                emit_barrier!($instruction, " ish")
            }
            (BarrierDomain::OuterShareable, BarrierAccess::Reads) => {
                emit_barrier!($instruction, " oshld")
            }
            (BarrierDomain::OuterShareable, BarrierAccess::Writes) => {
                emit_barrier!($instruction, " oshst")
            }
            (BarrierDomain::OuterShareable, BarrierAccess::All) => {
                emit_barrier!($instruction, " osh")
            }
            (BarrierDomain::FullSystem, BarrierAccess::Reads) => emit_barrier!($instruction, " ld"),
            (BarrierDomain::FullSystem, BarrierAccess::Writes) => {
                emit_barrier!($instruction, " st")
            }
            (BarrierDomain::FullSystem, BarrierAccess::All) => emit_barrier!($instruction, " sy"),
        }
    };
}

macro_rules! emit_barrier {
    ($instruction:literal, $option:literal) => {{
        // SAFETY: The operands are compile-time architectural barrier options;
        // neither instruction accesses the stack or changes condition flags.
        unsafe {
            asm!(
                concat!($instruction, $option),
                options(nostack, preserves_flags)
            )
        }
    }};
}

impl Barrier for Aarch64Barrier {
    #[inline]
    fn data_memory(domain: BarrierDomain, access: BarrierAccess) {
        execute_barrier!("dmb", domain, access);
    }

    #[inline]
    fn data_synchronization(domain: BarrierDomain, access: BarrierAccess) {
        execute_barrier!("dsb", domain, access);
    }

    #[inline]
    fn instruction_synchronization() {
        // SAFETY: ISB only synchronizes the current processing element.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
    }
}
