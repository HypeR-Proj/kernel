// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent debugging and symbolization primitives.

/// Reports an unrecoverable internal invariant through the binary panic policy.
///
/// Static reasons keep formatting temporaries off normal callers' stacks.
/// The non-inlined cold boundary constructs the panic only after failure.
/// This is never ordinary error handling. The kernel builds with panic=abort:
/// its panic handler captures a bounded reason and enters coordinated crash
/// handling without allocating, taking ordinary locks, or unwinding live owners.
/// Before crash output is ready, the handler records the failure when CPU
/// identity is available; it cannot safely assume that a console is mapped.
/// Keeping this boundary in core's panic machinery also lets portable mechanisms
/// report failures without depending on installed kernel services.
#[cold]
#[inline(never)]
#[track_caller]
#[expect(
    clippy::panic,
    reason = "unrecoverable invariants must reach the binary crash policy"
)]
pub fn invariant_failure(reason: &'static str) -> ! {
    panic!("internal invariant failure: {reason}")
}
