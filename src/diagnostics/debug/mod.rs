// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime kernel diagnostics.

#[cfg(feature = "runtime-symbols")]
pub mod symbols;

use hyper::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "runtime-symbols")]
pub mod kallsyms;

static READY: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    #[cfg(feature = "runtime-symbols")]
    Lookup(kallsyms::Error),
    #[cfg(feature = "runtime-symbols")]
    MissingSelfSymbol,
    #[cfg(feature = "runtime-symbols")]
    UnexpectedSelfSymbol,
}

/// Validates the runtime symbol table before diagnostics depend on it.
pub(crate) fn initialize() -> Result<(), InitializationError> {
    #[cfg(feature = "runtime-symbols")]
    initialize_symbols()?;
    READY.store(true, Ordering::Release);
    Ok(())
}

#[cfg(feature = "runtime-symbols")]
fn initialize_symbols() -> Result<(), InitializationError> {
    let lookup_address = kallsyms::lookup as *const () as usize;
    let symbol = kallsyms::lookup(lookup_address)
        .map_err(InitializationError::Lookup)?
        .ok_or(InitializationError::MissingSelfSymbol)?;
    if symbol.name != "hyper_kallsyms_lookup" || symbol.offset != 0 {
        return Err(InitializationError::UnexpectedSelfSymbol);
    }
    crate::pr_info!(
        "HypeR: kallsyms resolved {} at {:#x}",
        symbol.name,
        symbol.address
    );
    Ok(())
}

pub(crate) fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}
