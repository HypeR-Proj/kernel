// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only access to firmware-initialized real-time clocks.

mod goldfish;
pub use goldfish::Goldfish;

use hyper::hw::pl031 as reg;
use {crate::platform::MmioMappingError, crate::platform::PermanentMmioMapping};

pub struct Pl031 {
    mapping: PermanentMmioMapping,
}

impl Pl031 {
    pub fn bind(mapping: PermanentMmioMapping) -> Result<Self, MmioMappingError> {
        Ok(Self {
            mapping: mapping.validate_window(reg::REGISTER_WINDOW, 4)?,
        })
    }

    /// No reset, counter programming or IRQ is needed to read the clock.
    /// A disabled device has no established time and is not started here.
    pub fn seconds(&self) -> Option<u32> {
        if self.read(reg::CONTROL) & reg::ENABLED == 0 {
            return None;
        }
        Some(self.read(reg::DATA))
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: bind checked the permanent Device mapping's alignment and
        // register window. Both private callers use aligned, read-only offsets
        // in that window. PL031 synchronizes RTCDR across counter increments.
        unsafe { core::ptr::read_volatile((self.mapping.virtual_start() + offset) as *const u32) }
    }
}
