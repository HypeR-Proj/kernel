// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::memory::{AddressTranslation, VirtualMemoryLayout};
use hyper::mm::{PhysicalAddress, VirtualAddress};

use super::super::registers;

pub(super) const MMIO_BASE: u64 = registers::SV39_MMIO_BASE;
pub(super) const LINEAR_BASE: u64 = registers::SV39_LINEAR_BASE;
pub(crate) const KERNEL_BASE: u64 = registers::SV39_KERNEL_BASE;
pub(super) const KERNEL_STACK_BASE: u64 = registers::SV39_STACK_BASE;
pub(super) const KERNEL_STACK_ARENA_BASE: u64 = KERNEL_STACK_BASE + 2 * 1024 * 1024;
const REGION_SIZE: u64 = 64 * 1024 * 1024 * 1024;

pub struct Riscv64AddressTranslation;

impl AddressTranslation for Riscv64AddressTranslation {
    fn bootstrap_accessible_limit() -> u64 {
        REGION_SIZE
    }

    fn layout() -> VirtualMemoryLayout {
        VirtualMemoryLayout {
            mmio_base: MMIO_BASE,
            linear_base: LINEAR_BASE,
            kernel_base: KERNEL_BASE,
        }
    }

    fn linear_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        translate(LINEAR_BASE, physical)
    }

    fn mmio_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        translate(MMIO_BASE, physical)
    }
}

fn translate(base: u64, physical: PhysicalAddress) -> Option<VirtualAddress> {
    (physical.get() < REGION_SIZE)
        .then(|| base.checked_add(physical.get()))
        .flatten()
        .map(VirtualAddress::new)
}
