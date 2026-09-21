// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VHE `AArch64` virtual-address geometry.
//!
//! VHE gives the EL2&0 regime independent lower and upper ranges. Native
//! process mappings use the lower range through `TTBR0_EL2`; permanent kernel
//! mappings use the canonical upper range through `TTBR1_EL2`.

use hyper::hal::memory::{AddressTranslation, VirtualMemoryLayout};
use hyper::mm::{PhysicalAddress, VirtualAddress};

use super::super::address_layout::{AddressLayout, AddressRange};

pub(super) const HOST_LAYOUT: AddressLayout = super::super::address::STAGE1_LAYOUT;
const _: () = assert!(super::super::address_layout::PAGE_SIZE == hyper::mm::PAGE_SIZE);
const BOOTSTRAP_ACCESSIBLE_LIMIT: u64 = super::super::bootstrap_map::IDENTITY_LIMIT;
pub(super) type HostLayout = AddressLayout;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RootRegion {
    Lower,
    Upper,
}

pub struct Aarch64AddressTranslation;

impl AddressTranslation for Aarch64AddressTranslation {
    fn bootstrap_accessible_limit() -> u64 {
        BOOTSTRAP_ACCESSIBLE_LIMIT
    }

    fn layout() -> VirtualMemoryLayout {
        let layout = HOST_LAYOUT;
        VirtualMemoryLayout {
            mmio_base: layout.mmio().base(),
            linear_base: layout.linear().base(),
            kernel_base: layout.image().base(),
        }
    }

    fn linear_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        let layout = HOST_LAYOUT;
        translated_address(layout.linear(), physical)
    }

    fn mmio_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        let layout = HOST_LAYOUT;
        translated_address(layout.mmio(), physical)
    }
}

fn translated_address(range: AddressRange, physical: PhysicalAddress) -> Option<VirtualAddress> {
    range.alias(physical.get(), 1).map(VirtualAddress::new)
}
