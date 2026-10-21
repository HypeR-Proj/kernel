// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Local host-owned PMU register mechanisms. IRQ routing belongs to kernel policy.

#[cfg(CONFIG_ARCH_AARCH64)]
pub use crate::arch::aarch64::pmu::{available, initialize, overflowed, reload, start, stop};
