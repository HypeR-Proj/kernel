// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the same ECAM discovery/configuration path as physical assignment.

use super::*;

const CONFIG: u64 = 0x0040_1000_0000;
const MEMORY: u64 = 0x1000_0000;

thread_local! {
    static ECAM: Cell<usize> = const { Cell::new(0) };
}
struct EcamBarrier;
impl Barrier for EcamBarrier {
    fn data_memory(_: BarrierDomain, _: BarrierAccess) {
        ECAM.with(|base| {
            if base.get() == 0 {
                return;
            }
            for slot in [1, 2] {
                for (bar, mask) in [0xffff_c000, 0, 0, 0, 0, 0].iter().enumerate() {
                    // SAFETY: this test thread owns the permanent aligned RAM
                    // config image and no other thread can reach its address.
                    unsafe {
                        let word = (base.get() as *mut u32).add(((slot << 15) + 0x10) / 4 + bar);
                        if word.read_volatile() == u32::MAX {
                            word.write_volatile(*mask);
                        }
                    }
                }
            }
        });
    }
    fn data_synchronization(_: BarrierDomain, _: BarrierAccess) {}
    fn instruction_synchronization() {}
}

struct EcamHardware {
    config: Region,
    memory: Region,
}
impl EcamHardware {
    fn new() -> Self {
        let hardware = Self {
            config: Region::new(CONFIG, 1 << 20),
            memory: Region::new(MEMORY, 1 << 24),
        };
        for (slot, id) in [(1, 0x1048), (2, 0x1041)] {
            let base = slot << 15;
            hardware.config.write(base, (id << 16) | 0x1af4);
            hardware.config.write(base + 4, (1 << 20) | 6);
            hardware.config.write(base + 0x34, 0x40);
            hardware.config.write(base + 0x3c, 1 << 8);
            // Hide MSI-X and PCI_CFG; retain all four mandatory vendor caps.
            hardware.config.write(base + 0x40, 0x8000_5011);
            for (start, next, kind, offset, length) in [
                (0x50, 0x60, 1, 0, 56),
                (0x60, 0x74, 2, 0x1000, 0x1000),
                (0x74, 0x84, 3, 0x2000, 1),
                (0x84, 0x94, 4, 0x3000, 64),
                (0x94, 0, 5, 0, 16),
            ] {
                let cap_len = if kind == 2 { 20 } else { 16 };
                hardware.config.write(
                    base + start,
                    (kind << 24) | (cap_len << 16) | (next << 8) | 9,
                );
                hardware.config.write(base + start + 8, offset);
                hardware.config.write(base + start + 12, length);
            }
            hardware.config.write(base + 0x70, 4);
        }
        ECAM.with(|base| base.set(hardware.config.words as usize));
        hardware
    }
    fn discover(&self, route: u32) -> Result<Vec<Prepared>, Error> {
        Transport::discover_ecam::<EcamBarrier>(&nodes(route), self)
    }
}
impl Drop for EcamHardware {
    fn drop(&mut self) {
        ECAM.with(|base| base.set(0));
    }
}
impl DriverServices for EcamHardware {
    fn map_mmio(&self, resource: MmioResource) -> Result<PermanentMmioMapping, MmioMappingError> {
        for region in [&self.config, &self.memory] {
            if resource.start() >= region.start
                && resource.end() <= region.start + region.size as u64
            {
                let offset = (resource.start() - region.start) as usize;
                // SAFETY: checked subrange of the test's permanently leaked RAM.
                return unsafe {
                    PermanentMmioMapping::new(
                        resource,
                        VirtualAddress::new(region.pointer(offset) as u64),
                    )
                };
            }
        }
        Err(MmioMappingError::NotMapped)
    }
}
fn nodes(route: u32) -> Vec<PlatformDevice> {
    let mut tree = Tree::default();
    tree.begin("");
    tree.cells("#address-cells", &[2]);
    tree.cells("#size-cells", &[2]);
    tree.begin("memory@40000000");
    tree.property("device_type", b"memory\0");
    tree.cells("reg", &[0, 0x4000_0000, 0, 0x4000_0000]);
    tree.end();
    tree.begin("gic@8000000");
    tree.property("compatible", b"arm,gic-v3\0");
    tree.cells("reg", &[0, 0x0800_0000, 0, 0x10000]);
    tree.cells("phandle", &[1]);
    tree.cells("#interrupt-cells", &[3]);
    tree.cells("#address-cells", &[2]);
    tree.property("interrupt-controller", &[]);
    tree.end();
    tree.begin("smmu@9050000");
    tree.property("compatible", b"arm,smmu-v3\0");
    tree.cells("reg", &[0, 0x0905_0000, 0, 0x20000]);
    tree.cells("phandle", &[2]);
    tree.end();
    tree.begin("pcie@4010000000");
    tree.property("compatible", b"pci-host-ecam-generic\0");
    tree.property("device_type", b"pci\0");
    tree.property("dma-coherent", &[]);
    tree.cells("reg", &[(CONFIG >> 32) as u32, CONFIG as u32, 0, 1 << 20]);
    tree.cells("#address-cells", &[3]);
    tree.cells("#size-cells", &[2]);
    tree.cells("bus-range", &[0, 0]);
    tree.cells("iommu-map", &[0, route, 0, 256]);
    tree.cells(
        "ranges",
        &[0x0200_0000, 0, MEMORY as u32, 0, MEMORY as u32, 0, 1 << 24],
    );
    tree.cells("interrupt-map-mask", &[0xf800, 0, 0, 7]);
    tree.cells(
        "interrupt-map",
        &[
            0x800, 0, 0, 1, 1, 0, 0, 0, 4, 4, 0x1000, 0, 0, 1, 1, 0, 0, 0, 5, 4,
        ],
    );
    tree.end();
    tree.end();
    let blob = tree.finish();
    let mut scanner = DeviceScanner::for_dependency_graph(&[]);
    crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    crate::require_ok(scanner.finish())
}

#[test]
fn two_functions_have_distinct_streams_and_mediated_transport_configuration() {
    let hw = EcamHardware::new();
    let functions = crate::require_ok(hw.discover(2));
    assert_eq!(functions.len(), 2);
    for (index, function) in functions.iter().enumerate() {
        let transport = &function.transport;
        assert_eq!(transport.stream(), Some(((index + 1) * 8) as u32));
        assert_eq!(function.interrupt, 36 + index as u32);
        assert_eq!(hw.config.read(((index + 1) << 15) + 4) & 7, 0);
        assert_eq!(hw.config.read(((index + 1) << 15) + 0x40) >> 16, 0x4000);
        let mut state = transport.state(GUEST, 64);
        assert_eq!(transport.read(&mut state, 0x34, 4), Some(0x50));
        assert_eq!(transport.read(&mut state, 0x40, 4), Some(0));
        assert_eq!(transport.read(&mut state, 0x94, 4), Some(0));
        assert!(transport.write(&mut state, 0x40, 4, u32::MAX as u64));
        assert_eq!(transport.read(&mut state, 0x40, 4), Some(0));
        transport.activate(&mut state);
        assert!(transport.write(&mut state, 4, 2, 6));
        assert_eq!(hw.config.read(((index + 1) << 15) + 4) & 7, 6);
        assert!(transport.reset());
        transport.stop(&mut state);
        assert_eq!(hw.config.read(((index + 1) << 15) + 4) & 4, 0);
    }
}

#[test]
fn missing_stream_route_and_malformed_capabilities_fail_closed() {
    let hw = EcamHardware::new();
    assert!(matches!(hw.discover(3), Err(Error::Firmware)));
    assert_eq!(hw.config.read((1 << 15) + 4) & 4, 0);
    hw.config.write((1 << 15) + 0x50, 0x0110_5009); // cyclic list
    assert!(matches!(hw.discover(2), Err(Error::Firmware)));
    assert_eq!(hw.config.read((1 << 15) + 4) & 4, 0);
}
