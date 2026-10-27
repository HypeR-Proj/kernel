// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host bridge and MSI-controller authority from the firmware graph.

use {crate::platform::MmioResource, crate::platform::PlatformDevice};

fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn property_word(node: &PlatformDevice, name: &str) -> Option<u32> {
    let bytes = node.property(name)?;
    (bytes.len() == 4).then_some(())?;
    word(bytes, 0)
}
fn under(node: &str, parent: &str) -> bool {
    node.strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}
fn bridge(node: &PlatformDevice) -> bool {
    node.is_compatible("brcm,bcm2712-pcie") && property_word(node, "num-lanes") == Some(4)
}

/// The supported root port, its downstream tree and MSI controller are one
/// handoff domain, including when validation fails. No raw platform claim may
/// bypass the PCI owner through a firmware alias.
pub fn owns_handoff_node(nodes: &[PlatformDevice], node: &PlatformDevice) -> bool {
    nodes
        .iter()
        .filter(|candidate| bridge(candidate))
        .any(|owner| {
            node.id() == owner.id()
                || under(node.path(), owner.path())
                || property_word(owner, "msi-parent")
                    .is_some_and(|phandle| property_word(node, "phandle") == Some(phandle))
        })
}

pub(super) struct Topology<'a> {
    pub function: &'a PlatformDevice,
    pub bridge: &'a PlatformDevice,
    pub mip: &'a PlatformDevice,
    pub outbound: MmioResource,
    pub message_address: u64,
    pub host_irq: u32,
    pub dma: super::bcm2712::DmaPlan,
}

impl<'a> Topology<'a> {
    pub fn discover(nodes: &'a [PlatformDevice]) -> Result<Option<Self>, super::Error> {
        let mut bridges = nodes.iter().filter(|node| bridge(node));
        let Some(controller) = bridges.next() else {
            return Ok(None);
        };
        if bridges.next().is_some() {
            return Err(super::Error::Firmware);
        }
        let result = (|| {
            // The configured inbound window is a trusted-owner DMA translation,
            // not an IOMMU. Every later imported RAM page must fit it too.
            let mut memory_seen = false;
            for memory in nodes
                .iter()
                .filter(|node| node.property("device_type") == Some(b"memory\0"))
            {
                if memory.registers().is_empty()
                    || memory
                        .registers()
                        .iter()
                        .any(|range| range.end() > super::DMA_OFFSET)
                {
                    return None;
                }
                memory_seen = true;
            }
            if !memory_seen {
                return None;
            }
            let mip_phandle = property_word(controller, "msi-parent")?;
            let mip = nodes
                .iter()
                .find(|node| property_word(node, "phandle") == Some(mip_phandle))?;
            if !mip.is_compatible("brcm,bcm2712-mip")
                || property_word(mip, "brcm,msi-offset").unwrap_or(0) != 0
            {
                return None;
            }
            let ranges = mip.property("msi-ranges")?;
            if ranges.len() != 20
                || word(ranges, 4)? != 0
                || word(ranges, 12)? != 1
                || word(ranges, 16)? != 64
            {
                return None;
            }
            let parent = word(ranges, 0)?;
            if !nodes.iter().any(|node| {
                property_word(node, "phandle") == Some(parent)
                    && node.kernel_claimed()
                    && (node.is_compatible("arm,gic-400") || node.is_compatible("arm,gic-v3"))
            }) {
                return None;
            }
            let mip_regs = mip.property("reg")?;
            if mip_regs.len() != 32 || word(mip_regs, 24)? != 0 || word(mip_regs, 28)? != 4096 {
                return None;
            }
            let message_address =
                (u64::from(word(mip_regs, 16)?) << 32) | u64::from(word(mip_regs, 20)?);
            let host_irq = word(ranges, 8)?.checked_add(32)?;
            let (_, outbound) = controller.pci_memory()?;
            let dma = super::bcm2712::DmaPlan::from_firmware(
                controller.property("dma-ranges")?,
                outbound,
                message_address,
                mip.registers().first()?.start(),
            )?;
            // A firmware child may describe the function's subsidiary buses.
            // Matching its path is optional; PCI VID/DID is read from hardware.
            let function = nodes
                .iter()
                .find(|node| {
                    node.path()
                        .rsplit_once('/')
                        .is_some_and(|(parent, _)| parent == controller.path())
                })
                .unwrap_or(controller);
            Some(Self {
                function,
                bridge: controller,
                mip,
                outbound,
                message_address,
                host_irq,
                dma,
            })
        })();
        result.map(Some).ok_or(super::Error::Firmware)
    }
}
