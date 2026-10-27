// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded PCI memory windows retained by early firmware discovery.

#[derive(Clone, Copy)]
pub(crate) struct PciWindow {
    bcm2712: super::bcm2712::PciWindow,
    ecam: bool,
}

impl PciWindow {
    pub(crate) const EMPTY: Self = Self {
        bcm2712: super::bcm2712::PciWindow::EMPTY,
        ecam: false,
    };

    pub(crate) fn property(&mut self, name: &str, value: &[u8]) {
        self.bcm2712.property(name, value);
        if name == "compatible" {
            self.ecam = value
                .split(|byte| *byte == 0)
                .any(|item| item == b"pci-host-ecam-generic");
        }
    }

    pub(crate) fn mapping_size(self, tag: u32, bus: u64, size: u64) -> Option<u64> {
        // Keep a bounded non-prefetchable 32-bit aperture, not firmware's
        // potentially hundreds of GiB of optional 64-bit PCI address space.
        // BAR admission must verify placement against this retained interval.
        if self.ecam
            && tag == 0x0200_0000
            && bus.checked_add(size).is_some_and(|end| end <= 1 << 32)
        {
            Some(size.min(8 * 1024 * 1024))
        } else {
            self.bcm2712.mapping_size(tag, bus, size)
        }
    }
}
