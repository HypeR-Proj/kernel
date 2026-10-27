// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Controller reservations applied before PCI function preparation.

use hyper_drivers::platform::PlatformDevice;

pub(crate) fn host_owned(nodes: &[PlatformDevice], node: &PlatformDevice) -> bool {
    // Even unsupported translation providers must remain outside assignable
    // resource bundles. Otherwise a future isolated master could be bypassed by
    // claiming its controller through the generic firmware/MMIO interface.
    node.property("#iommu-cells").is_some()
        || node.is_compatible("arm,smmu-v3")
        || node.is_compatible("brcm,bcm2712-iommu")
        || node.is_compatible("pci-host-ecam-generic")
        // A supported PCI handoff already has a kernel owner for its bridge
        // and MSI controller. Reserving them here would make that owner
        // conflict with itself before it can prepare the function. The final
        // catalogue reservation still bars raw claims on the entire PCI tree
        // and its MSI controller, including after failed preparation.
        || (node.property("msi-controller").is_some()
            && !hyper_drivers::pci::owns_handoff_node(nodes, node))
}
