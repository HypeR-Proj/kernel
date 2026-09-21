// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected runtime atomic-backend diagnostics.
//!
//! Architecture bootstrap selects and publishes the executable atomic backend
//! before shared Rust state is used. This facade exposes immutable diagnostics;
//! it does not select the backend or replace Rust/LLVM atomic operations.

/// Runtime-selected atomic implementation admitted for every online CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities(crate::arch::memory::AtomicCapabilities);

impl Capabilities {
    pub const fn backend_name(self) -> &'static str {
        self.0.backend_name()
    }
}

pub fn capabilities() -> Capabilities {
    Capabilities(crate::arch::memory::atomic_capabilities())
}
