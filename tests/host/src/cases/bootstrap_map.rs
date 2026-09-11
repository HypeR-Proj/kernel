// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::bootstrap_map::{IDENTITY_LIMIT, populate};
use crate::registers::{BOOT_DEVICE_BLOCK_FLAGS, BOOT_NORMAL_BLOCK_FLAGS};
use hyper::platform::{CpuList, PhysicalRange, PlatformInfo, RegionList};

fn platform(ram: u64, size: u64, mmio: u64) -> PlatformInfo {
    let mut result = PlatformInfo {
        cpus: CpuList::new(),
        memory: RegionList::new(),
        reserved: RegionList::new(),
        no_map: RegionList::new(),
        mmio: RegionList::new(),
        dtb_size: 0,
    };
    crate::require_ok(
        result
            .memory
            .insert(crate::require_some(PhysicalRange::new(ram, size))),
    );
    crate::require_ok(
        result
            .mmio
            .insert(crate::require_some(PhysicalRange::new(mmio, 4096))),
    );
    result
}

#[test]
fn maps_firmware_ram_and_high_peripherals_with_distinct_attributes() {
    let mut table = [0; 512];
    assert!(populate(
        &mut table,
        &platform(0x40000000, 0x20000000, 0x09000000)
    ));
    assert_eq!(table[0], BOOT_DEVICE_BLOCK_FLAGS);
    assert_eq!(table[1], 0x40000000 | BOOT_NORMAL_BLOCK_FLAGS);
    assert!(populate(&mut table, &platform(0, 8 << 30, 0x107d001000)));
    assert_eq!(table[0], BOOT_NORMAL_BLOCK_FLAGS);
    assert_eq!(table[7], (7 << 30) | BOOT_NORMAL_BLOCK_FLAGS);
    assert_eq!(table[65], (65 << 30) | BOOT_DEVICE_BLOCK_FLAGS);
}

#[test]
fn refuses_mixed_attributes_and_out_of_range_resources() {
    let mut table = [0; 512];
    assert!(!populate(&mut table, &platform(0, 0x20000000, 0x20000000)));
    assert!(!populate(
        &mut table,
        &platform(IDENTITY_LIMIT, 4096, 0x09000000)
    ));
    assert!(!populate(&mut table, &platform(0, 4096, IDENTITY_LIMIT)));
}
