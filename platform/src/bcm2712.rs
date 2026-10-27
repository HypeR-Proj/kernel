// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Early firmware facts needed to retain a bounded `PCIe` handoff mapping.

#[derive(Clone, Copy, Default)]
pub(crate) struct PciWindow {
    compatible: bool,
    lanes: u32,
}

impl PciWindow {
    pub(crate) const EMPTY: Self = Self {
        compatible: false,
        lanes: 0,
    };

    pub(crate) fn property(&mut self, name: &str, value: &[u8]) {
        match name {
            "compatible" => {
                self.compatible = value
                    .split(|byte| *byte == 0)
                    .any(|item| item == b"brcm,bcm2712-pcie")
            }
            "num-lanes" => self.lanes = value.try_into().map(u32::from_be_bytes).unwrap_or(0),
            _ => {}
        }
    }

    /// Limit supported firmware layouts instead of mapping the whole 4 GiB
    /// outbound aperture. The PCI host driver admits only functions whose
    /// complete BAR set fits this early mapping and validates actual placement.
    pub(crate) fn mapping_size(self, pci_tag: u32, bus: u64, size: u64) -> Option<u64> {
        const LIMIT: u64 = 8 * 1024 * 1024;
        (self.compatible && self.lanes == 4 && pci_tag == 0x0200_0000 && bus == 0 && size >= LIMIT)
            .then_some(LIMIT)
    }
}
