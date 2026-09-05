// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` guest-entry local-IRQ mask contract test.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    InitiallyUnmasked,
    PreparationUnmasked,
}

pub(super) fn run() -> Result<(), Error> {
    if crate::hal::irq::local_enabled() {
        return Err(Error::InitiallyUnmasked);
    }
    crate::hal::vm::prepare_interrupts_for_entry();
    if crate::hal::irq::local_enabled() {
        // Preserve the test harness's masked-IRQ postcondition on failure.
        crate::hal::irq::mask_local();
        return Err(Error::PreparationUnmasked);
    }
    Ok(())
}
