// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` EL2 stage-1 descriptor encoding and classification.
//!
//! This module owns the architectural bit layout for table, block, and page
//! descriptors. Page-table walkers consume its closed classification helpers
//! rather than duplicating masks or VHE execute-never rules. It performs no
//! memory access and owns no table pages.

#[cfg(CONFIG_CRASH_CONSOLE)]
use hyper::hal::memory::{Stage1Mapping, Stage1MemoryType};

use super::super::super::registers;

#[derive(Clone, Copy)]
pub(super) struct MappingFlags {
    memory: MemoryType,
    writable: bool,
    executable: bool,
}

impl MappingFlags {
    pub(super) const NORMAL_RW: Self = Self {
        memory: MemoryType::Normal,
        writable: true,
        executable: false,
    };

    pub(super) const NORMAL_RO: Self = Self {
        memory: MemoryType::Normal,
        writable: false,
        executable: false,
    };

    pub(super) const NORMAL_RX: Self = Self {
        memory: MemoryType::Normal,
        writable: false,
        executable: true,
    };

    pub(super) const DEVICE_RW: Self = Self {
        memory: MemoryType::Device,
        writable: true,
        executable: false,
    };

    fn attribute_bits(self) -> u64 {
        let mut bits = registers::STAGE1_DESC_ACCESS_FLAG;
        bits |= match self.memory {
            MemoryType::Normal => {
                registers::STAGE1_DESC_ATTR_NORMAL | registers::STAGE1_DESC_INNER_SHAREABLE
            }
            MemoryType::Device => registers::STAGE1_DESC_OUTER_SHAREABLE,
        };
        if !self.writable {
            bits |= registers::STAGE1_DESC_AP_READ_ONLY;
        }
        // Host mappings never permit EL0 execution. PXN additionally blocks
        // privileged instruction fetches from non-executable mappings.
        bits |= registers::STAGE1_DESC_UXN;
        if !self.executable {
            bits |= registers::STAGE1_DESC_PXN;
        }

        bits
    }
}

#[derive(Clone, Copy)]
enum MemoryType {
    Normal,
    Device,
}

pub(super) const fn table_index(address: u64, level: usize) -> usize {
    registers::stage1_table_index(address, level, super::super::super::address::STAGE1_VA_BITS)
}

pub(super) fn best_mapping_level(
    virtual_address: u64,
    physical_address: u64,
    remaining: u64,
) -> usize {
    for (level, &size) in registers::STAGE1_LEVEL_SIZES_4K.iter().enumerate().skip(1) {
        if virtual_address.is_multiple_of(size)
            && physical_address.is_multiple_of(size)
            && remaining >= size
        {
            return level;
        }
    }
    3
}

/// Recognizes the table encoding used at levels 0 through 2.
///
/// Level 3 uses the same low bits for a page descriptor, so walkers must test
/// [`is_leaf`] first when the current level is not already constrained.
pub(super) const fn is_table(descriptor: u64) -> bool {
    descriptor & registers::TRANSLATION_DESC_TYPE_MASK == registers::STAGE1_DESC_TABLE_OR_PAGE
}

pub(super) const fn is_leaf(descriptor: u64, level: usize) -> bool {
    let kind = descriptor & registers::TRANSLATION_DESC_TYPE_MASK;
    (level < 3 && kind == registers::STAGE1_DESC_BLOCK)
        || (level == 3 && kind == registers::STAGE1_DESC_TABLE_OR_PAGE)
}

/// Returns the output address field without interpreting block alignment.
///
/// A block consumer must additionally clear the low bits implied by its level.
pub(super) const fn output_address(descriptor: u64) -> u64 {
    descriptor & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT
}

pub(super) const fn table(child: u64) -> u64 {
    child | registers::STAGE1_DESC_TABLE_OR_PAGE
}

pub(super) fn leaf(physical_address: u64, level: usize, flags: MappingFlags) -> u64 {
    let kind = if level == 3 {
        registers::STAGE1_DESC_TABLE_OR_PAGE
    } else {
        registers::STAGE1_DESC_BLOCK
    };
    (physical_address & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT)
        | flags.attribute_bits()
        | kind
}

#[cfg(CONFIG_CRASH_CONSOLE)]
pub(super) fn decode_mapping(descriptor: u64, level: usize, address: u64) -> Option<Stage1Mapping> {
    if !is_leaf(descriptor, level) {
        return None;
    }
    let size = registers::STAGE1_LEVEL_SIZES_4K[level];
    let physical_start = output_address(descriptor) & !(size - 1);
    let attribute = descriptor & registers::STAGE1_DESC_ATTR_INDEX_MASK;
    Some(Stage1Mapping {
        virtual_start: address & !(size - 1),
        physical_start,
        size,
        readable: true,
        writable: descriptor & registers::STAGE1_DESC_AP_READ_ONLY == 0,
        executable: descriptor & registers::STAGE1_DESC_PXN == 0,
        memory_type: if attribute == registers::STAGE1_DESC_ATTR_NORMAL {
            Stage1MemoryType::Normal
        } else if attribute == registers::STAGE1_DESC_ATTR_DEVICE {
            Stage1MemoryType::Device
        } else {
            Stage1MemoryType::Unknown
        },
    })
}
