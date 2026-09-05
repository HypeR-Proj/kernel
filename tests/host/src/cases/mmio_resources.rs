// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Firmware-described MMIO ranges and permanent mapping capability contracts.

use hyper::{
    drivers::platform::{MmioMappingError, MmioResource, PermanentMmioMapping},
    mm::VirtualAddress,
    platform::PhysicalRange,
};

fn resource(start: u64, size: u64) -> MmioResource {
    let range = crate::require_some(PhysicalRange::new(start, size));
    // SAFETY: Tests mint metadata-only resources and never expose them as
    // authority over host physical memory.
    unsafe { MmioResource::from_physical_range(range) }
}

#[test]
fn distinguishes_a_description_from_a_validated_mapping() {
    let described = resource(0x0900_0000, 0x1000);
    assert_eq!(described.start(), 0x0900_0000);
    assert_eq!(described.end(), 0x0900_1000);

    // SAFETY: This test inspects capability metadata only and does not
    // dereference the representative permanent virtual interval.
    let mapped = crate::require_ok(unsafe {
        PermanentMmioMapping::new(described, VirtualAddress::new(0xffff_8000_0900_0000))
    });
    let validated = crate::require_ok(mapped.validate_window(0x1000, 4));
    assert_eq!(validated.resource(), described);
    assert_eq!(validated.virtual_start(), 0xffff_8000_0900_0000);
}

#[test]
fn rejects_invalid_driver_windows_before_register_access() {
    // SAFETY: These metadata-only mappings are never dereferenced.
    let short = crate::require_ok(unsafe {
        PermanentMmioMapping::new(resource(0x1000, 8), VirtualAddress::new(0x2000))
    });
    assert_eq!(
        short.validate_window(9, 1),
        Err(MmioMappingError::WindowTooSmall)
    );
    assert_eq!(
        short.validate_window(8, 3),
        Err(MmioMappingError::InvalidAlignment)
    );

    // SAFETY: This metadata-only mapping is never dereferenced.
    let misaligned = crate::require_ok(unsafe {
        PermanentMmioMapping::new(resource(0x1001, 8), VirtualAddress::new(0x2001))
    });
    assert_eq!(
        misaligned.validate_window(8, 4),
        Err(MmioMappingError::Misaligned)
    );
}

#[test]
fn rejects_a_virtual_interval_that_wraps_usize() {
    // SAFETY: Construction must reject this interval before it can become a
    // mapping capability.
    let result = unsafe {
        PermanentMmioMapping::new(
            resource(0x1000, 8),
            VirtualAddress::new(usize::MAX.saturating_sub(3) as u64),
        )
    };
    assert_eq!(result, Err(MmioMappingError::AddressOverflow));
}
