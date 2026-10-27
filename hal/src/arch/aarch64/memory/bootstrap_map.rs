// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Transient identity-map planning before the permanent, page-granular map.

use {
    crate::arch::aarch64::registers::BOOT_DEVICE_BLOCK_FLAGS,
    crate::arch::aarch64::registers::BOOT_NORMAL_BLOCK_FLAGS,
};
use {hyper::mm::ranges::PhysicalRange, hyper_platform::PlatformInfo};

pub const IDENTITY_LIMIT: u64 = 1 << 39;
const BLOCK_SIZE: u64 = 1 << 30;

/// RAM is Normal memory even when the Image is loaded below 1 GiB (Pi 5).
/// Every other block is Device/XN, including the BCM2712 peripheral window.
/// Reject mixed RAM/MMIO blocks instead of silently giving devices cacheable
/// aliases. These coarse mappings live only until the final tables activate.
pub fn populate(table: &mut [u64; 512], platform: &PlatformInfo) -> bool {
    if platform.memory.as_slice().is_empty()
        || platform
            .memory
            .as_slice()
            .iter()
            .chain(platform.mmio.as_slice())
            .any(|r| r.end() > IDENTITY_LIMIT)
    {
        return false;
    }
    for (index, entry) in table.iter_mut().enumerate() {
        let base = (index as u64) * BLOCK_SIZE;
        let Some(block) = PhysicalRange::new(base, BLOCK_SIZE) else {
            return false;
        };
        let ram = platform.memory.as_slice().iter().any(|r| r.overlaps(block));
        if ram && platform.mmio.as_slice().iter().any(|r| r.overlaps(block)) {
            return false;
        }
        *entry = base
            | if ram {
                BOOT_NORMAL_BLOCK_FLAGS
            } else {
                BOOT_DEVICE_BLOCK_FLAGS
            };
    }
    true
}
