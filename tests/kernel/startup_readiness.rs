// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Verifies the prerequisites published before the full fatal path is enabled.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Crash,
    Debug,
    Memory,
    MemoryProtection,
    RuntimeExceptions,
    Scheduler,
}

pub(super) fn run() -> Result<(), Error> {
    if !crate::kernel::mm::is_ready() {
        return Err(Error::Memory);
    }
    if !crate::kernel::mm::memory_protection_active() {
        return Err(Error::MemoryProtection);
    }
    if !crate::kernel::debug::is_ready() {
        return Err(Error::Debug);
    }
    if !crate::kernel::task::is_ready() {
        return Err(Error::Scheduler);
    }
    if !crate::kernel::irq::exceptions_ready() {
        return Err(Error::RuntimeExceptions);
    }
    if !crate::kernel::crash::is_ready() {
        return Err(Error::Crash);
    }
    Ok(())
}
