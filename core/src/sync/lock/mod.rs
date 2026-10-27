// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Locking primitives and interrupt-masking composition.

mod interrupt;
mod sharded;
mod spin;

pub use interrupt::{InterruptMaskGuard, InterruptSpinLock};
pub use sharded::InterruptShardedLock;
pub use spin::SpinLock;
