// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Run-scoped FP/SIMD ownership for Native EL0 and guest vCPUs.
//!
//! The soft-float kernel never borrows FP registers. Vector entry gates access
//! but retains the active lower-world register image across direct returns.
//! First use restores the exact owner; actual scheduling or ownership changes
//! save that image and scrub physical registers. A suspended Native syscall
//! retains only its saved Thread context and a frame with FP return disabled.
//! No physical bank residency survives withdrawn CPU publication.

use super::{exception::ExceptionFrame, registers};

#[cfg(feature = "kernel-self-test")]
use hyper::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "kernel-self-test")]
static RESTORES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kernel-self-test")]
static SAVES: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "kernel-self-test")]
pub(crate) fn fp_state_counts_for_test() -> (usize, usize) {
    (
        RESTORES.load(Ordering::Relaxed),
        SAVES.load(Ordering::Relaxed),
    )
}

#[repr(C, align(16))]
pub(super) struct State {
    vectors: [[u64; 2]; 32],
    control: u64,
    status: u64,
}

impl State {
    pub(super) const fn zeroed() -> Self {
        Self {
            vectors: [[0; 2]; 32],
            control: 0,
            status: 0,
        }
    }
}

const _: () = {
    assert!(core::mem::offset_of!(State, control) == 512);
    assert!(core::mem::offset_of!(State, status) == 520);
    assert!(core::mem::size_of::<State>() == 528);
};

unsafe extern "C" {
    fn aarch64_fp_reset();
    fn aarch64_fp_restore(state: *const State);
    fn aarch64_fp_save_and_clear(state: *mut State);
}

/// Clears firmware state once before this CPU can run any lower-world owner.
///
/// # Safety
/// CPU admission must have established FP/SIMD support. Local exceptions must
/// remain masked, and no Native or guest register image may be resident yet.
pub(super) unsafe fn initialize_cpu() {
    // SAFETY: The admission caller establishes capability and exclusive bank ownership.
    unsafe { aarch64_fp_reset() };
}

/// Resolves a first-use trap without advancing the interrupted instruction.
///
/// The caller has matched the frame to the currently published pinned owner;
/// only architecture exception dispatch may call this with local IRQs masked.
pub(super) fn restore(state: &State, frame: &mut ExceptionFrame) -> Result<(), ()> {
    if frame.return_cptr & registers::CPTR_EL2_FPEN_ENABLED != 0 {
        return Err(());
    }
    // SAFETY: `state` is the current lower-world owner's initialized image.
    // No prior owner remains resident; kernel code uses the soft-float ABI.
    // The leaf temporarily admits FP with IRQs masked and gates it before
    // returning, so neither a kernel continuation nor IRQ can borrow it.
    unsafe { aarch64_fp_restore(state) };
    #[cfg(feature = "kernel-self-test")]
    RESTORES.fetch_add(1, Ordering::Relaxed);
    frame.return_cptr |= registers::CPTR_EL2_FPEN_ENABLED;
    Ok(())
}

/// Retires residency before unwind, Native suspension, or guest IRQ scheduling.
pub(super) fn save(state: &mut State, frame: &ExceptionFrame) {
    if frame.return_cptr & registers::CPTR_EL2_FPEN_ENABLED == 0 {
        return;
    }
    // SAFETY: The exception frame identifies the still-resident current owner
    // and IRQs remain masked. No Rust kernel code may access FP state. The
    // save completes before ownership can move/drop; clearing hardware state
    // also prevents a later owner from inheriting retired register contents.
    unsafe { aarch64_fp_save_and_clear(state) };
    #[cfg(feature = "kernel-self-test")]
    SAVES.fetch_add(1, Ordering::Relaxed);
}
