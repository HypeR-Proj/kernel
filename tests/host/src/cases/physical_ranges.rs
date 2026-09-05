// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Physical-range validation, merging, and fixed-capacity behavior.

use hyper::platform::{PhysicalRange, RegionList};

#[test]
fn rejects_empty_and_overflowing_ranges() {
    assert!(PhysicalRange::new(0, 0).is_none());
    assert!(PhysicalRange::new(u64::MAX, 2).is_none());
    assert_eq!(crate::require_some(PhysicalRange::new(4, 8)).end(), 12);
}

#[test]
fn coalesces_adjacent_ranges_and_preserves_capacity_on_failure() {
    let mut regions = RegionList::<2>::new();
    crate::require_ok(regions.insert(crate::require_some(PhysicalRange::new(0x3000, 0x1000))));
    crate::require_ok(regions.insert(crate::require_some(PhysicalRange::new(0x1000, 0x2000))));
    assert_eq!(
        regions.as_slice(),
        &[crate::require_some(PhysicalRange::new(0x1000, 0x3000))]
    );

    crate::require_ok(regions.insert(crate::require_some(PhysicalRange::new(0x8000, 0x1000))));
    assert!(
        regions
            .insert(crate::require_some(PhysicalRange::new(0xa000, 0x1000)))
            .is_err()
    );
    assert_eq!(regions.len(), 2);
}
