// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod controller;
pub(crate) mod gic;

use core::arch::asm;

use hyper::hal::interrupt::InterruptMask;

use crate::arch::aarch64::registers;

/// Local `AArch64` interrupt-mask policy used by IRQ-safe kernel locks.
pub struct LocalInterruptMask;

impl InterruptMask for LocalInterruptMask {
    type State = u64;

    #[inline]
    fn save_and_disable() -> Self::State {
        let state: u64;
        // SAFETY: Reading DAIF and masking local exceptions does not access
        // memory and is valid while the kernel executes at EL2.
        unsafe {
            asm!(
                "mrs {state}, daif",
                "msr daifset, #{all}",
                state = out(reg) state,
                all = const registers::DAIFSET_ALL,
                // Exception masking is also a compiler barrier: an interrupt
                // handler may observe ordinary memory on either side.
                options(nostack, preserves_flags)
            );
        }
        state
    }

    #[inline]
    fn restore(state: Self::State) {
        // SAFETY: `state` was captured from DAIF on this processing element
        // immediately before the matching lock acquisition.
        unsafe {
            asm!(
                "msr daif, {state}",
                state = in(reg) state,
                // Restoring exceptions must remain after the protected memory
                // accesses, not merely after the hardware lock release.
                options(nostack, preserves_flags)
            );
        }
    }

    fn wait_for_lock_owner() {
        // SGIs cannot be taken with DAIF.I set. Drain the durable mailbox
        // through its opaque poll-safe callback so synchronous RPC cannot be
        // blocked by a target contending on an IRQ-safe lock.
        crate::arch::irq::service_kernel_rpc();
        core::hint::spin_loop();
    }
}

/// Enables local IRQ exceptions while leaving FIQ, `SError`, and debug masked.
///
/// Runtime vectors and the interrupt dispatcher must be installed first.
pub fn enable_irq() {
    // SAFETY: DAIFClr with immediate 2 clears only the IRQ mask bit.
    unsafe {
        asm!(
            "msr daifclr, #{irq}",
            irq = const registers::DAIF_IRQ,
            options(nostack, preserves_flags)
        )
    };
}

/// Masks ordinary local IRQ exceptions without changing other exception masks.
///
/// This operation pairs with [`enable_irq`] across controlled context-entry
/// transitions. IRQ-safe lexical critical sections must use
/// [`LocalInterruptMask`] so they restore the exact prior state.
#[allow(dead_code)]
pub fn mask_irq() {
    // SAFETY: DAIFSet with immediate 2 sets only the IRQ mask bit. This
    // instruction remains a compiler memory boundary because interrupt
    // handlers may observe ordinary memory around the transition.
    unsafe {
        asm!(
            "msr daifset, #{irq}",
            irq = const registers::DAIF_IRQ,
            options(nostack, preserves_flags)
        )
    };
}

/// Reports whether ordinary IRQ exceptions are currently unmasked.
pub fn irq_enabled() -> bool {
    let state: u64;
    // SAFETY: Reading DAIF has no side effects and is valid at EL2.
    unsafe {
        asm!(
            "mrs {state}, daif",
            state = out(reg) state,
            options(nomem, nostack, preserves_flags)
        );
    }
    state & registers::SPSR_I == 0
}

/// Masks every local exception class for irreversible crash handling.
pub fn disable_all() {
    // SAFETY: Crash handling never restores execution after applying the mask.
    unsafe {
        asm!(
            "msr daifset, #{all}",
            all = const registers::DAIFSET_ALL,
            options(nostack, preserves_flags)
        )
    };
}
