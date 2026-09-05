// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime physical-page and heap allocators.

mod buddy;
pub mod heap;

pub use buddy::{BuddyAllocator, BuddyError, BuddyStats, MAX_ORDER, MemoryHandoff};
