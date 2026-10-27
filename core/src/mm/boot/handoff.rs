// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable boot-to-runtime memory map.

use super::{MAX_BOOT_RESERVATIONS, MAX_MEMORY_REGIONS};
use crate::mm::ranges::{PhysicalRange, RegionList};

#[derive(Clone, Copy)]
pub struct MemoryHandoff {
    memory: RegionList<MAX_MEMORY_REGIONS>,
    reserved: RegionList<MAX_BOOT_RESERVATIONS>,
}

impl MemoryHandoff {
    /// Returns the memory and reservation maps for runtime initialization.
    pub fn into_regions(
        self,
    ) -> (
        RegionList<MAX_MEMORY_REGIONS>,
        RegionList<MAX_BOOT_RESERVATIONS>,
    ) {
        (self.memory, self.reserved)
    }

    pub(crate) const fn new(
        memory: RegionList<MAX_MEMORY_REGIONS>,
        reserved: RegionList<MAX_BOOT_RESERVATIONS>,
    ) -> Self {
        Self { memory, reserved }
    }

    pub fn memory(&self) -> &[PhysicalRange] {
        self.memory.as_slice()
    }

    pub fn reserved(&self) -> &[PhysicalRange] {
        self.reserved.as_slice()
    }
}
