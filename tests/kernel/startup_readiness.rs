// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Verifies the prerequisites published before the full fatal path is enabled.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Crash,
    PrematureTopology,
    PrematureSealing,
    Debug,
    Memory,
    MemoryProtection,
    RuntimeExceptions,
    Scheduler,
}

pub(super) fn run() -> Result<(), Error> {
    if !crate::mm::is_ready() {
        return Err(Error::Memory);
    }
    if !crate::mm::memory_protection_active() {
        return Err(Error::MemoryProtection);
    }
    if !crate::diagnostics::debug::is_ready() {
        return Err(Error::Debug);
    }
    if !crate::task::is_ready() {
        return Err(Error::Scheduler);
    }
    if !crate::irq::exceptions_ready() {
        return Err(Error::RuntimeExceptions);
    }
    if !crate::diagnostics::crash::is_ready() {
        return Err(Error::Crash);
    }
    Ok(())
}

/// Early rejection must not touch mappings needed by later CPU admission.
pub(super) fn before_smp() -> Result<(), Error> {
    if crate::cpu::frozen_topology().is_some() {
        return Err(Error::PrematureTopology);
    }
    if crate::mm::seal_address_space() != Err(crate::mm::FinalizationError::CpuTopologyUnavailable)
    {
        return Err(Error::PrematureSealing);
    }
    Ok(())
}
