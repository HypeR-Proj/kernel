// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fatal architecture-exception entry policy.

use core::fmt;

/// Enters coordinated fail-stop handling with an owned architecture snapshot.
///
/// Exception entry must call this with local interrupts masked. The raw
/// architecture frame remains owned by the backend; `context` is an owned
/// diagnostic snapshot and this function never returns.
pub(crate) fn fatal(context: crate::hal::exception::CrashContext, reason: fmt::Arguments<'_>) -> ! {
    crate::diagnostics::crash::fatal_context(context, reason)
}
