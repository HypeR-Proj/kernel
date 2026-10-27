// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional crash-monitor lifecycle adapter.
//!
//! This module is the only coordination-to-monitor edge. It keeps feature
//! selection and emergency-console availability out of the fail-stop state
//! machine; when the feature is disabled both calls compile to no-ops.

use super::report::StopSummary;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitializationError {
    #[cfg(feature = "crash-console")]
    Monitor(super::monitor::InitializationError),
}

pub(super) fn initialize() -> Result<(), InitializationError> {
    #[cfg(feature = "crash-console")]
    super::monitor::initialize().map_err(InitializationError::Monitor)?;
    Ok(())
}

pub(super) fn run(owner: usize, stop: StopSummary) {
    #[cfg(feature = "crash-console")]
    if crate::diagnostics::log::crash_console_available() {
        crate::diagnostics::log::emergency(format_args!(
            "crash console enabled; entering interactive monitor"
        ));
        super::monitor::run(owner, stop);
    } else {
        crate::diagnostics::log::emergency(format_args!(
            "crash console enabled but no emergency console is available"
        ));
    }

    #[cfg(not(feature = "crash-console"))]
    let _ = (owner, stop);
}
