// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral fatal-crash facade.
//!
//! Crash policy coordinates a single diagnostic owner, freezes other CPUs,
//! publishes debugger-visible snapshots without locks, and renders bounded,
//! allocation-free diagnostics. Architecture entry code supplies snapshots but
//! does not own this policy. Runtime subsystems access only this facade; state
//! publication, reporting, unwinding, and the optional monitor remain private.

mod console;
mod coordination;
mod fixed_text;
#[cfg(feature = "crash-console")]
mod monitor;
mod report;
mod state;
mod unwind;

pub(crate) use coordination::is_ready;
pub(crate) use coordination::{
    EarlyInitializationError, InitializationError, early_initialize, fatal, fatal_context,
    initialize,
};
pub use coordination::{is_stop_interrupt, panic, stop_this_cpu};
