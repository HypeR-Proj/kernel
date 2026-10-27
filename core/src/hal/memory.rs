// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::mm::{PhysicalAddress, VirtualAddress};

/// Linker-derived physical layout of the loaded kernel image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelImageLayout {
    pub physical_start: u64,
    pub text_size: u64,
    pub rodata_size: u64,
    pub total_size: u64,
}

/// Architecture-selected permanent virtual address regions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualMemoryLayout {
    pub mmio_base: u64,
    pub linear_base: u64,
    pub kernel_base: u64,
}

/// Architecture-neutral memory type reported by a live stage-1 page-table walk.
#[cfg(feature = "crash-console")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage1MemoryType {
    Normal,
    Device,
    Unknown,
}

/// One leaf mapping containing a queried stage-1 virtual address.
#[cfg(feature = "crash-console")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stage1Mapping {
    pub virtual_start: u64,
    pub physical_start: u64,
    pub size: u64,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
    pub memory_type: Stage1MemoryType,
}

/// Address translation capabilities consumed by architecture-neutral kernel
/// and driver initialization code.
pub trait AddressTranslation {
    /// Highest physical address reachable through the early boot mapping.
    fn bootstrap_accessible_limit() -> u64;
    fn layout() -> VirtualMemoryLayout;
    fn linear_address(physical: PhysicalAddress) -> Option<VirtualAddress>;
    fn mmio_address(physical: PhysicalAddress) -> Option<VirtualAddress>;
}
