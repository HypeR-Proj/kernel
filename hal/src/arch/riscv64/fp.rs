// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Run-scoped F/D residency. The IMAC kernel never borrows floating registers.

use super::registers;
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

#[repr(C)]
pub(super) struct State {
    registers: [u64; 32],
    control: u32,
    padding: u32,
}
impl State {
    pub(super) const fn zeroed() -> Self {
        Self {
            registers: [0; 32],
            control: 0,
            padding: 0,
        }
    }
}
const _: () = {
    assert!(core::mem::offset_of!(State, control) == 256);
    assert!(core::mem::size_of::<State>() == 264);
};
unsafe extern "C" {
    fn riscv64_fp_reset();
    fn riscv64_fp_restore(state: *const State);
    fn riscv64_fp_save_and_clear(state: *mut State);
}

/// Clears firmware state after this hart's F/D baseline has been admitted.
pub(super) fn initialize_local() {
    // SAFETY: CPU admission verified F/D, holds IRQs masked, and has not yet
    // admitted any lower-world execution on this hart.
    unsafe { riscv64_fp_reset() };
}

/// Restores a Native owner's bank for a first-use retry at the same PC.
/// A genuine illegal instruction faults again and is then contained.
pub(super) fn restore(state: &State, status: &mut u64) -> Result<(), ()> {
    if *status & registers::SSTATUS_FS_MASK != 0 {
        return Err(());
    }
    restore_for_run(state);
    *status |= registers::SSTATUS_FS_CLEAN;
    Ok(())
}

/// Loads an exclusively owned bank, leaving physical FS disabled until SRET.
pub(super) fn restore_for_run(state: &State) {
    // SAFETY: Native exception entry or the guest runner qualified this
    // exclusively borrowed, pinned owner with IRQs masked. The leaf enables
    // FS only while loading and disables it before returning to IMAC Rust.
    unsafe { riscv64_fp_restore(state) };
    #[cfg(feature = "kernel-self-test")]
    RESTORES.fetch_add(1, Ordering::Relaxed);
}

/// Saves and scrubs resident state before typed anchor unwind can schedule.
pub(super) fn save(state: &mut State, status: u64) {
    if status & registers::SSTATUS_FS_MASK == 0 {
        return;
    }
    // SAFETY: The stopped, interrupt-masked owner still has the physical bank;
    // all privileged code is IMAC. No owner survives outside the run anchor.
    unsafe { riscv64_fp_save_and_clear(state) };
    #[cfg(feature = "kernel-self-test")]
    SAVES.fetch_add(1, Ordering::Relaxed);
}
