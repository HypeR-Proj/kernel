// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU8, Ordering};

use super::registers;

unsafe extern "C" {
    /// Runtime selector consumed by compiler-builtins outline atomic helpers.
    static mut __aarch64_have_lse_atomics: u8;
}

/// Runtime atomic capabilities selected for the admitted processing elements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicCapabilities {
    lse: bool,
}

impl AtomicCapabilities {
    pub const fn backend_name(self) -> &'static str {
        if self.lse { "LSE" } else { "LL/SC" }
    }
}

/// Selects the fastest atomic backend supported by the boot CPU.
///
/// This must run before Rust code performs an outlined atomic read-modify-write
/// operation. SMP admission rejects a secondary that does not support the
/// globally selected backend before it reaches shared kernel state.
pub fn initialize() -> AtomicCapabilities {
    let features: u64;
    // SAFETY: ID_AA64ISAR0_EL1 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "mrs {features}, id_aa64isar0_el1",
            features = out(reg) features,
            options(nomem, nostack, preserves_flags)
        );
    }
    let atomic =
        (features >> registers::ID_AA64ISAR0_ATOMIC_SHIFT) & registers::ID_AA64ISAR0_ATOMIC_MASK;
    let lse = atomic >= registers::ID_AA64ISAR0_ATOMIC_LSE;

    // Release ordering publishes the selector to every processing element that
    // observes it with an acquire load, so a secondary never acts on a stale
    // value while racing with bootstrap.
    selector().store(u8::from(lse), Ordering::Release);
    AtomicCapabilities { lse }
}

pub fn capabilities() -> AtomicCapabilities {
    AtomicCapabilities {
        lse: selector().load(Ordering::Acquire) != 0,
    }
}

/// Borrows the compiler-builtins selector as an atomic cell.
///
/// The byte is shared mutable state written once by bootstrap and read by every
/// secondary CPU, so all Rust accesses go through this cell to keep them free of
/// data races.
fn selector() -> &'static AtomicU8 {
    // SAFETY: compiler-builtins defines this hidden one-byte selector with
    // static lifetime, and AtomicU8 has the same layout as u8. Rust code only
    // ever touches it through this reference.
    unsafe { AtomicU8::from_ptr(addr_of_mut!(__aarch64_have_lse_atomics)) }
}

/// Checks that the calling CPU can execute the globally selected backend.
pub fn current_cpu_supports_selected_backend() -> bool {
    if !capabilities().lse {
        return true;
    }
    let features: u64;
    // SAFETY: ID_AA64ISAR0_EL1 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "mrs {features}, id_aa64isar0_el1",
            features = out(reg) features,
            options(nomem, nostack, preserves_flags)
        );
    }
    let atomic =
        (features >> registers::ID_AA64ISAR0_ATOMIC_SHIFT) & registers::ID_AA64ISAR0_ATOMIC_MASK;
    atomic >= registers::ID_AA64ISAR0_ATOMIC_LSE
}
