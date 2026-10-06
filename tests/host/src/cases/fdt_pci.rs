// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! PCI address tags and the bounded PCI firmware handoff aperture.

use super::*;

fn add_string(blob: &mut Vec<u8>, name: &[u8]) -> u32 {
    let offset = u32::from_be_bytes(crate::require_ok(blob[32..36].try_into()));
    blob.extend_from_slice(name);
    blob.push(0);
    let total = blob.len() as u32;
    blob[4..8].copy_from_slice(&total.to_be_bytes());
    blob[32..36].copy_from_slice(&(offset + name.len() as u32 + 1).to_be_bytes());
    offset
}

fn pci_tree(pci: bool, tag: u32, lanes: u32) -> Vec<u8> {
    let mut blob = qemu_like_dtb();
    let lanes_name = add_string(&mut blob, b"num-lanes");
    let mut nodes = Vec::new();
    begin_node(&mut nodes, b"pcie@1000120000");
    property(&mut nodes, 0, &3u32.to_be_bytes());
    property(&mut nodes, 15, &2u32.to_be_bytes());
    property(&mut nodes, 31, b"brcm,bcm2712-pcie\0");
    if pci {
        property(&mut nodes, 42, b"pci\0");
    }
    property(&mut nodes, lanes_name, &lanes.to_be_bytes());
    property(&mut nodes, 27, &cells(&[0x10, 0x120000, 0, 0x9310]));
    property(
        &mut nodes,
        54,
        &cells(&[tag, 0, 0, 0x1f, 0, 0, 0xffff_fffc]),
    );
    begin_node(&mut nodes, b"rp1");
    property(&mut nodes, 31, b"simple-bus\0");
    property(&mut nodes, 0, &2u32.to_be_bytes());
    property(&mut nodes, 15, &2u32.to_be_bytes());
    property(
        &mut nodes,
        54,
        &cells(&[0xc0, 0x40000000, tag, 0, 0, 0, 0x410000]),
    );
    begin_node(&mut nodes, b"ethernet@100000");
    property(&mut nodes, 31, b"raspberrypi,rp1-gem\0");
    property(&mut nodes, 27, &cells(&[0xc0, 0x40100000, 0, 0x4000]));
    push_u32(&mut nodes, FDT_END_NODE);
    push_u32(&mut nodes, FDT_END_NODE);
    // PCI configuration and port-I/O resources must not become CPU MMIO.
    for (name, tag) in [
        (b"function@0".as_slice(), 0x00010000),
        (b"io@80", 0x01000000),
    ] {
        begin_node(&mut nodes, name);
        property(&mut nodes, 31, b"test,pci-resource\0");
        property(&mut nodes, 27, &cells(&[tag, 0, 0x80, 0, 0x20]));
        push_u32(&mut nodes, FDT_END_NODE);
    }
    push_u32(&mut nodes, FDT_END_NODE);
    add_root_nodes(blob, &nodes)
}

fn discover(blob: &[u8]) -> (hyper::platform::PlatformInfo, Vec<PlatformDevice>) {
    let mut scanner = DeviceScanner::new(&[]);
    let platform = crate::require_ok(fdt::discover_from_bytes_with(blob, &mut scanner));
    (platform, crate::require_ok(scanner.finish()))
}

#[test]
fn rp1_registers_translate_through_typed_pci_bus_and_map_bounded_aperture() {
    let (platform, devices) = discover(&pci_tree(true, 0x02000000, 4));
    let gem = crate::require_some(
        devices
            .iter()
            .find(|node| node.is_compatible("raspberrypi,rp1-gem")),
    );
    assert_eq!(gem.registers().len(), 1);
    assert_eq!(gem.registers()[0].start(), 0x1f00100000);
    assert_eq!(gem.registers()[0].size(), 0x4000);
    let bridge = crate::require_some(
        devices
            .iter()
            .find(|node| node.is_compatible("brcm,bcm2712-pcie")),
    );
    assert_eq!(bridge.registers().len(), 1);
    assert_eq!(bridge.registers()[0].start(), 0x1000120000);
    let (bus, window) = crate::require_some(bridge.pci_memory());
    assert_eq!(bus, 0);
    assert_eq!(window.start(), 0x1f00000000);
    assert_eq!(window.size(), 8 * 1024 * 1024);
    assert!(
        platform
            .mmio
            .as_slice()
            .iter()
            .any(|range| { range.start() == window.start() && range.size() == window.size() })
    );
    for node in devices
        .iter()
        .filter(|node| node.is_compatible("test,pci-resource"))
    {
        assert!(node.registers().is_empty());
    }
    assert!(
        !platform
            .mmio
            .as_slice()
            .iter()
            .any(|range| range.start() == 0x80)
    );
}

#[test]
fn unsupported_three_cell_bus_cannot_publish_pci_resources() {
    let (platform, devices) = discover(&pci_tree(false, 0x02000000, 4));
    assert!(
        !platform
            .mmio
            .as_slice()
            .iter()
            .any(|range| { range.start() >= 0x1f00000000 && range.start() < 0x2000000000 })
    );
    assert!(devices.iter().all(|node| node.pci_memory().is_none()));
    assert!(
        devices
            .iter()
            .filter(|node| node.is_compatible("raspberrypi,rp1-gem"))
            .all(|node| node.registers().is_empty())
    );
}

#[test]
fn prefetchable_pci_space_is_translated_without_admitting_handoff_window() {
    let (_, devices) = discover(&pci_tree(true, 0x43000000, 4));
    let gem = crate::require_some(
        devices
            .iter()
            .find(|node| node.is_compatible("raspberrypi,rp1-gem")),
    );
    assert_eq!(gem.registers()[0].start(), 0x1f00100000);
    assert!(devices.iter().all(|node| node.pci_memory().is_none()));
}

#[test]
fn external_single_lane_pcie_does_not_inherit_x4_mapping_policy() {
    let (_, devices) = discover(&pci_tree(true, 0x02000000, 1));
    assert!(devices.iter().all(|node| node.pci_memory().is_none()));
}
