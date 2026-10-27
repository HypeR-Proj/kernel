// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime physical-page and heap allocators.

mod buddy;
pub mod heap;
pub(crate) mod policy;

pub use {
    crate::mm::allocator::buddy::BuddyAllocator, crate::mm::allocator::buddy::BuddyError,
    crate::mm::allocator::buddy::BuddyStats, crate::mm::allocator::buddy::MAX_ORDER,
};

pub use hyper::mm::boot::MemoryHandoff;
