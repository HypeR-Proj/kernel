// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Early allocation before the runtime heap is available.

mod allocator;

pub use allocator::{BootAllocator, BootAllocatorError, BootMemoryStats, MAX_BOOT_RESERVATIONS};

/// Capacity of the early physical memory map.
pub const MAX_MEMORY_REGIONS: usize = 8;
/// Capacity of firmware-reserved physical memory ranges.
pub const MAX_RESERVED_REGIONS: usize = 32;

mod handoff;
pub use handoff::MemoryHandoff;
