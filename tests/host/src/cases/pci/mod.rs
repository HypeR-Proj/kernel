// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the real firmware-handoff driver with permanent RAM-backed MMIO.
//! This checks bus ownership and mediation, not electrical timing or DMA isolation.

use std::cell::Cell;

mod ecam;

#[path = "../../../../../src/device/iommu/ownership.rs"]
mod controller_ownership;

use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain, hyper::mm::VirtualAddress, hyper_drivers::pci::Error,
    hyper_drivers::pci::LinkFailure, hyper_drivers::pci::LinkState, hyper_drivers::pci::Prepared,
    hyper_drivers::pci::Transport, hyper_drivers::platform::DeviceScanner,
    hyper_drivers::platform::DriverServices, hyper_drivers::platform::MmioMappingError,
    hyper_drivers::platform::MmioResource, hyper_drivers::platform::PermanentMmioMapping,
    hyper_drivers::platform::PlatformDevice, hyper_platform::fdt,
};

const PCIE: u64 = 0x1000120000;
const MIP: u64 = 0x1000130000;
const RP1: u64 = 0x1f00000000;
const GEM: usize = 0x100000;
const APBS: usize = 0x108000;
const TABLE: usize = 0x400000;
const VECTOR: usize = TABLE + 6 * 16;
const GUEST: u64 = 0x0b80_0000;
const COMMAND: usize = 0x8004;
const CAPABILITY: usize = 0x8040;
const DMA_RANGES: &[u32] = &[
    0x0200_0000,
    0,
    0,
    0x1f,
    0,
    0,
    0x40_0000,
    0x4300_0000,
    0x10,
    0,
    0,
    0,
    0x10,
    0,
    0x0300_0000,
    0xff,
    0xffff_f000,
    0x10,
    0x13_0000,
    0,
    0x1000,
];

fn inbound_offsets(index: usize) -> (usize, usize) {
    assert!(index < 10);
    if index < 3 {
        (0x402c + index * 8, 0x40ac + index * 8)
    } else {
        (0x40d4 + (index - 3) * 8, 0x410c + (index - 3) * 8)
    }
}

thread_local! {
    static STATUS_REGISTER: Cell<usize> = const { Cell::new(0) };
    static FIXED_BUS_NUMBERS: Cell<Option<u32>> = const { Cell::new(None) };
    static FIXED_REGISTER: Cell<Option<(usize, u32)>> = const { Cell::new(None) };
    static TRACE_OUTBOUND: Cell<Option<usize>> = const { Cell::new(None) };
    static TRACE_DMA: Cell<bool> = const { Cell::new(false) };
    static DMA_PREVIOUS_MASK: Cell<u16> = const { Cell::new(0) };
    static DMA_EMPTY_SEEN: Cell<bool> = const { Cell::new(false) };
    static DMA_BAD_ORDER: Cell<bool> = const { Cell::new(false) };
}

struct TestBarrier;
impl Barrier for TestBarrier {
    fn data_memory(domain: BarrierDomain, access: BarrierAccess) {
        assert_eq!(domain, BarrierDomain::FullSystem);
        assert_eq!(access, BarrierAccess::All);
        // PCI Status.CAP_LIST is read-only. A RAM image must restore it after
        // a command-register write; otherwise it would falsely lose MSI-X.
        STATUS_REGISTER.with(|address| {
            let address = address.get();
            if address != 0 {
                let register = address as *mut u32;
                // SAFETY: each test thread installs its own leaked MMIO word;
                // all driver and model accesses are serialized on that thread.
                unsafe {
                    register.write_volatile(register.read_volatile() | (1 << 20));
                    // Model a root port which refuses the bus-number write.
                    if let Some(value) = FIXED_BUS_NUMBERS.with(Cell::get) {
                        register.sub((COMMAND - 0x18) / 4).write_volatile(value);
                    }
                    let bridge = register.sub(COMMAND / 4);
                    if let Some((offset, value)) = FIXED_REGISTER.with(Cell::get) {
                        bridge.add(offset / 4).write_volatile(value);
                    }
                    if let Some(index) = TRACE_OUTBOUND.with(Cell::get) {
                        let limits = bridge.add((0x4070 + index * 4) / 4).read_volatile();
                        let high = bridge.add((0x4080 + index * 8) / 4).read_volatile();
                        let last = bridge.add((0x4084 + index * 8) / 4).read_volatile();
                        let base = (u64::from(high & 0xff) << 32)
                            | (u64::from((limits >> 4) & 0xfff) << 20);
                        let end = (u64::from(last & 0xff) << 32)
                            + (u64::from(limits >> 20) + 1) * 0x10_0000;
                        // Every intermediate decode is empty or contained in
                        // the original firmware range or final reserved range.
                        assert!(
                            base >= end
                                || (base >= 0x1c_0000_0000 && end <= 0x1c_4000_0000)
                                || (base >= RP1 && end <= RP1 + 0x50_0000)
                        );
                        if base != 0x1c_0000_0000 || end != 0x1c_4000_0000 {
                            assert_eq!(register.read_volatile() & 4, 0);
                        }
                    }
                    if TRACE_DMA.with(Cell::get) {
                        let mask = (0..10).fold(0, |mask, index| {
                            let (bar, _) = inbound_offsets(index);
                            mask | if bridge.add(bar / 4).read_volatile() & 31 != 0 {
                                1 << index
                            } else {
                                0
                            }
                        });
                        let previous = DMA_PREVIOUS_MASK.with(|old| old.replace(mask));
                        if mask == 0 {
                            DMA_EMPTY_SEEN.with(|seen| seen.set(true));
                        }
                        if (mask & !previous != 0 && !DMA_EMPTY_SEEN.with(Cell::get))
                            || (mask != previous && register.read_volatile() & 4 != 0)
                        {
                            DMA_BAD_ORDER.with(|bad| bad.set(true));
                        }
                    }
                    for (index, mask) in [0xffffc000, 0xffc00000, 0xffff0000, 0, 0, 0]
                        .iter()
                        .enumerate()
                    {
                        let bar = register.add(3 + index);
                        if bar.read_volatile() == u32::MAX {
                            bar.write_volatile(*mask);
                        }
                    }
                };
            }
        });
    }
    fn data_synchronization(_: BarrierDomain, _: BarrierAccess) {}
    fn instruction_synchronization() {}
}

struct Region {
    start: u64,
    words: *mut u32,
    size: usize,
}
impl Region {
    fn new(start: u64, size: usize) -> Self {
        let words = Box::leak(vec![0_u32; size / 4].into_boxed_slice()).as_mut_ptr();
        Self { start, words, size }
    }
    fn pointer(&self, offset: usize) -> *mut u32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.size);
        // SAFETY: offset was checked against the leaked aligned allocation.
        unsafe { self.words.add(offset / 4) }
    }
    fn read(&self, offset: usize) -> u32 {
        // SAFETY: pointer checks bounds; tests serialize driver/model accesses.
        unsafe { self.pointer(offset).read_volatile() }
    }
    fn write(&self, offset: usize, value: u32) {
        // SAFETY: same single-threaded permanent backing as read.
        unsafe { self.pointer(offset).write_volatile(value) };
    }
}

struct Hardware {
    pcie: Region,
    mip: Region,
    rp1: Region,
}
impl Hardware {
    fn new() -> Self {
        let this = Self {
            pcie: Region::new(PCIE, 0x9310),
            mip: Region::new(MIP, 0xc0),
            rp1: Region::new(RP1, 0x420000),
        };
        // One inherited bus-1/function-0 endpoint and a live root-port link.
        this.pcie.write(0x4068, 0xb0);
        this.pcie.write(0x18, 0x0001_0100);
        this.pcie.write(0x8000, 0x0001_1de4);
        this.pcie.write(COMMAND, (1 << 20) | 6);
        this.pcie.write(0x8010, TABLE as u32); // BAR0: MSI-X table.
        this.pcie.write(0x8014, 0); // BAR1: function MMIO.
        this.pcie.write(0x8018, 0x410000); // BAR2: SRAM.
        this.pcie.write(0x8034, 0x40);
        this.pcie.write(CAPABILITY, (63 << 16) | 0x11);
        this.pcie.write(0x8044, 0);
        this.pcie.write(0x8048, 0x800);
        // Outbound CPU 0x1f00000000..0x2000000000 -> PCI 0..4GiB.
        this.outbound_window(0, RP1, 0, 0x1_0000_0000);
        // Inbound PCI 64..128GiB -> RAM 0..64GiB, and MSI page -> MIP.
        this.pcie.write(0x402c, 21);
        this.pcie.write(0x4030, 0x10);
        this.pcie.write(0x40ac, 1);
        this.pcie.write(0x4034, 0xffff_f01c);
        this.pcie.write(0x4038, 0xff);
        this.pcie.write(0x40b4, 0x130001);
        this.pcie.write(0x40b8, 0x10);
        this.rp1.write(GEM + 0x294, 1 << 23); // One queue, 44-bit DMA.
        STATUS_REGISTER.with(|address| address.set(this.pcie.pointer(COMMAND) as usize));
        this
    }
    fn discover(&self) -> Result<Option<Prepared>, Error> {
        Transport::discover::<TestBarrier>(&firmware(0x4000_0000), self)
    }
    fn prepared(&self) -> Prepared {
        crate::require_some(crate::require_ok(self.discover()))
    }
    fn dma_enabled(&self) -> bool {
        self.pcie.read(COMMAND) & 4 != 0
    }
    fn outbound_window(&self, index: usize, cpu: u64, bus: u64, size: u64) {
        assert!(index < 4 && size != 0);
        assert!(cpu.is_multiple_of(0x10_0000) && size.is_multiple_of(0x10_0000));
        let last = cpu + size - 1;
        self.pcie.write(0x400c + index * 8, bus as u32);
        self.pcie.write(0x4010 + index * 8, (bus >> 32) as u32);
        self.pcie.write(
            0x4070 + index * 4,
            (((cpu >> 20) as u32 & 0xfff) << 4) | (((last >> 20) as u32 & 0xfff) << 20),
        );
        self.pcie.write(0x4080 + index * 8, (cpu >> 32) as u32);
        self.pcie.write(0x4084 + index * 8, (last >> 32) as u32);
    }
    fn firmware_outbound(&self, index: usize) {
        // Root-port window and BAR0 are from the physical Pi5 boot report.
        // BAR1/BAR2 complete the fixture with the standard contiguous RP1 layout.
        for other in 0..4 {
            self.pcie.write(0x4080 + other * 8, 0);
            self.pcie.write(0x4084 + other * 8, 0);
            self.pcie.write(0x4070 + other * 4, 0x10);
        }
        self.outbound_window(index, 0x1c_0000_0000, 0x8000_0000, 0x4000_0000);
        for (index, address) in [0x8041_0000, 0x8000_0000, 0x8040_0000]
            .into_iter()
            .enumerate()
        {
            self.pcie.write(0x8010 + index * 4, address);
        }
    }
}
impl Drop for Hardware {
    fn drop(&mut self) {
        STATUS_REGISTER.with(|address| address.set(0));
        FIXED_BUS_NUMBERS.with(|value| value.set(None));
        FIXED_REGISTER.with(|value| value.set(None));
        TRACE_OUTBOUND.with(|value| value.set(None));
        TRACE_DMA.with(|value| value.set(false));
        DMA_PREVIOUS_MASK.with(|value| value.set(0));
        DMA_EMPTY_SEEN.with(|value| value.set(false));
        DMA_BAD_ORDER.with(|value| value.set(false));
    }
}
impl DriverServices for Hardware {
    fn map_mmio(&self, resource: MmioResource) -> Result<PermanentMmioMapping, MmioMappingError> {
        for region in [&self.pcie, &self.mip, &self.rp1] {
            if resource.start() >= region.start
                && resource.end() <= region.start + region.size as u64
            {
                let offset = (resource.start() - region.start) as usize;
                // SAFETY: the complete checked subrange lies in leaked aligned
                // RAM, which remains live for every retained driver mapping.
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

#[derive(Default)]
struct Tree {
    structure: Vec<u8>,
    strings: Vec<u8>,
}
impl Tree {
    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }
    fn pad(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }
    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
    }
    fn end(&mut self) {
        self.word(2);
    }
    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3);
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.pad();
    }
    fn cells(&mut self, name: &str, words: &[u32]) {
        self.property(
            name,
            &words
                .iter()
                .flat_map(|word| word.to_be_bytes())
                .collect::<Vec<_>>(),
        );
    }
    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let total = 56 + self.structure.len() + self.strings.len();
        let header = [
            0xd00d_feed,
            total as u32,
            56,
            (56 + self.structure.len()) as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ];
        let mut blob: Vec<u8> = header.iter().flat_map(|word| word.to_be_bytes()).collect();
        blob.extend_from_slice(&[0; 16]);
        blob.extend(self.structure);
        blob.extend(self.strings);
        blob
    }
}

fn firmware(memory_size: u64) -> Vec<PlatformDevice> {
    firmware_with_dma(memory_size, Some(DMA_RANGES))
}

fn firmware_with_dma(memory_size: u64, dma: Option<&[u32]>) -> Vec<PlatformDevice> {
    // These addresses, sizes and topology mirror the pinned Pi5 C0/D0 trees.
    // Clock/PHY properties are application policy and need not be duplicated.
    let mut tree = Tree::default();
    tree.begin("");
    tree.cells("#address-cells", &[2]);
    tree.cells("#size-cells", &[2]);
    tree.begin("memory@0");
    tree.property("device_type", b"memory\0");
    tree.cells(
        "reg",
        &[0, 0, (memory_size >> 32) as u32, memory_size as u32],
    );
    tree.end();
    tree.begin("gic@8000000");
    tree.property("compatible", b"arm,gic-400\0");
    tree.cells("phandle", &[1]);
    tree.cells("reg", &[0, 0x8000000, 0, 0x1000]);
    tree.end();
    tree.begin("pcie@1000120000");
    tree.property("compatible", b"brcm,bcm2712-pcie\0");
    tree.property("device_type", b"pci\0");
    tree.cells("num-lanes", &[4]);
    tree.cells("reg", &[0x10, 0x120000, 0, 0x9310]);
    tree.cells("#address-cells", &[3]);
    tree.cells("#size-cells", &[2]);
    tree.cells("ranges", &[0x02000000, 0, 0, 0x1f, 0, 0, 0xfffffffc]);
    tree.cells("msi-parent", &[2]);
    tree.property("msi-controller", b"");
    if let Some(dma) = dma {
        tree.cells("dma-ranges", dma);
    }
    tree.begin("rp1");
    tree.property("compatible", b"simple-bus\0");
    tree.cells("#address-cells", &[2]);
    tree.cells("#size-cells", &[2]);
    tree.cells("ranges", &[0xc0, 0x40000000, 0x02000000, 0, 0, 0, 0x410000]);
    for (name, compatible, reg) in [
        (
            "ethernet@100000",
            b"raspberrypi,rp1-gem\0".as_slice(),
            &[0xc0, 0x40100000, 0, 0x4000][..],
        ),
        (
            "clocks@18000",
            b"raspberrypi,rp1-clocks\0",
            &[0xc0, 0x40018000, 0, 0x10038][..],
        ),
        (
            "gpio@d0000",
            b"raspberrypi,rp1-gpio\0",
            &[
                0xc0, 0x400d0000, 0, 0xc000, 0xc0, 0x400e0000, 0, 0xc000, 0xc0, 0x400f0000, 0,
                0xc000,
            ][..],
        ),
    ] {
        tree.begin(name);
        tree.property("compatible", compatible);
        tree.cells("reg", reg);
        if name.starts_with("ethernet") {
            tree.cells("interrupts", &[6, 4]);
        }
        tree.end();
    }
    tree.end();
    tree.end();
    tree.begin("msi-controller@1000130000");
    tree.property("compatible", b"brcm,bcm2712-mip\0");
    tree.property("msi-controller", b"");
    tree.cells("phandle", &[2]);
    tree.cells(
        "reg",
        &[0x10, 0x130000, 0, 0xc0, 0xff, 0xfffff000, 0, 0x1000],
    );
    tree.cells("msi-ranges", &[1, 0, 128, 1, 64]);
    tree.end();
    tree.end();
    let blob = tree.finish();
    let mut scanner = DeviceScanner::for_dependency_graph(&[]);
    crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let first = crate::require_ok(scanner.finish());
    let gic = crate::require_some(first.iter().find(|node| node.is_compatible("arm,gic-400"))).id();
    let claims = [Some(gic)];
    let mut scanner = DeviceScanner::for_dependency_graph(&claims);
    crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    crate::require_ok(scanner.finish())
}

#[test]
fn pci_handoff_does_not_conflict_with_its_own_msi_controllers() {
    let nodes = firmware(0x4000_0000);
    let controllers: Vec<_> = nodes
        .iter()
        .filter(|node| node.property("msi-controller").is_some())
        .collect();
    assert_eq!(controllers.len(), 2);
    for node in controllers {
        assert!(
            !controller_ownership::host_owned(&nodes, node),
            "{} must remain available to the mediated PCI preparation path",
            node.path()
        );
        // The final catalogue reservation still excludes these controllers
        // from raw userspace firmware claims, even if preparation fails.
        assert!(hyper_drivers::pci::owns_handoff_node(&nodes, node));
    }
}

#[test]
fn unrelated_msi_and_translation_controllers_remain_host_owned() {
    let mut tree = Tree::default();
    tree.begin("");
    tree.cells("#address-cells", &[2]);
    tree.cells("#size-cells", &[2]);
    tree.begin("memory@0");
    tree.property("device_type", b"memory\0");
    tree.cells("reg", &[0, 0, 0, 0x4000_0000]);
    tree.end();
    for (name, compatible, property) in [
        ("its", "arm,gic-v3-its", "msi-controller"),
        ("mip", "brcm,bcm2712-mip", "msi-controller"),
        ("smmu", "arm,smmu-v3", "#iommu-cells"),
        ("iommu", "brcm,bcm2712-iommu", "#iommu-cells"),
        ("unknown-iommu", "test,iommu", "#iommu-cells"),
        ("ecam", "pci-host-ecam-generic", "#address-cells"),
    ] {
        tree.begin(name);
        tree.property("compatible", format!("{compatible}\0").as_bytes());
        tree.cells(property, &[0]);
        tree.end();
    }
    tree.end();
    let blob = tree.finish();
    let mut scanner = DeviceScanner::for_dependency_graph(&[]);
    crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let nodes = crate::require_ok(scanner.finish());
    let controllers: Vec<_> = nodes
        .iter()
        .filter(|node| !node.compatibles().is_empty())
        .collect();
    assert_eq!(controllers.len(), 6);
    for node in controllers {
        assert!(
            controller_ownership::host_owned(&nodes, node),
            "{}",
            node.path()
        );
        assert!(!hyper_drivers::pci::owns_handoff_node(&nodes, node));
    }
}

#[test]
fn link_failures_never_touch_endpoint_configuration() {
    for (status, buses, failure) in [
        (0, 0, LinkFailure::NotRootPort),
        (0x80, 0, LinkFailure::LinkDown),
        (0xa0, 0x10100, LinkFailure::LinkDown),
        (0x90, 0x10100, LinkFailure::LinkDown),
    ] {
        let hw = Hardware::new();
        hw.pcie.write(0x4068, status);
        hw.pcie.write(0x18, buses);
        hw.pcie.write(0x9000, 0x1234);
        let expected = Error::Link(LinkState {
            failure,
            bridge: PCIE,
            status,
            bus_numbers: buses,
            control: 0,
        });
        assert!(matches!(hw.discover(), Err(error) if error == expected));
        assert_eq!(hw.pcie.read(0x9000), 0x1234);
        assert_eq!(hw.pcie.read(0x18), buses);
        assert!(hw.dma_enabled());
    }
}

#[test]
fn live_link_without_firmware_bus_numbers_is_enumerated() {
    for buses in [0, 0xa500_0000, 0x0000_0100, 0x0001_0200, 0x0001_0102] {
        let hw = Hardware::new();
        // Observed on Pi 5: root-port mode, PHY and data link up, bus numbers 0.
        hw.pcie.write(0x4068, 0x0003_e0b0);
        hw.pcie.write(0x4064, 4);
        hw.pcie.write(0x18, buses);
        let windows: Vec<_> = (0x400c..0x402c)
            .step_by(4)
            .chain((0x4070..0x40a0).step_by(4))
            .map(|o| (o, hw.pcie.read(o)))
            .collect();
        let prepared = hw.prepared();
        assert_eq!(prepared.transport.identity(), 0x0001_1de4);
        assert_eq!(hw.pcie.read(0x18), (buses & 0xff00_0000) | 0x0001_0100);
        assert_eq!(hw.pcie.read(0x9000), 1 << 20);
        assert!(!hw.dma_enabled());
        for (offset, original) in windows {
            assert_eq!(hw.pcie.read(offset), original);
        }
    }
}

#[test]
fn valid_firmware_bus_routing_is_preserved() {
    for buses in [0x0001_0100, 0x00ff_0100, 0xa508_0400] {
        let hw = Hardware::new();
        hw.pcie.write(0x18, buses);
        let _prepared = hw.prepared();
        assert_eq!(hw.pcie.read(0x18), buses);
        assert_eq!(hw.pcie.read(0x9000), ((buses >> 8) & 0xff) << 20);
        assert!(!hw.dma_enabled());
    }
}

#[test]
fn firmware_cpu_window_is_relocated_into_the_reserved_aperture() {
    for window in 0..4 {
        let hw = Hardware::new();
        hw.firmware_outbound(window);
        TRACE_OUTBOUND.with(|value| value.set(Some(window)));
        let prepared = hw.prepared();
        for (index, offset) in [0x410000, 0, 0x400000].into_iter().enumerate() {
            assert_eq!(
                crate::require_some(prepared.bars[index])
                    .mapping
                    .resource()
                    .start(),
                RP1 + offset
            );
            assert_eq!(
                u64::from(hw.pcie.read(0x8010 + index * 4)),
                0x8000_0000 + offset
            );
        }
        assert_eq!(hw.pcie.read(0x400c + window * 8), 0x8000_0000);
        assert_eq!(hw.pcie.read(0x4010 + window * 8), 0);
        // Only the 5 MiB prefix containing the BARs needs CPU decoding.
        assert_eq!(hw.pcie.read(0x4070 + window * 4), 0x0040_0000);
        assert_eq!(hw.pcie.read(0x4080 + window * 8), 0x1f);
        assert_eq!(hw.pcie.read(0x4084 + window * 8), 0x1f);
        assert!(!hw.dma_enabled());
        // The real MSI table access must now use BAR0's relocated CPU address.
        assert_eq!(hw.rp1.read(0x410000 + 6 * 16), 0xffff_f000);
        assert_eq!(hw.rp1.read(0x410000 + 6 * 16 + 4), 0xff);
    }
}

#[test]
fn unsafe_relocation_leaves_outbound_registers_unchanged() {
    for scenario in 0..5 {
        let hw = Hardware::new();
        hw.firmware_outbound(0);
        match scenario {
            // Two firmware windows alias the endpoint BARs.
            0 => hw.outbound_window(1, 0x1d_0000_0000, 0x8000_0000, 0x50_0000),
            // Unrelated PCI targets still conflict with source/destination decode.
            1 => hw.outbound_window(1, 0x1c_0040_0000, 0x9000_0000, 0x10_0000),
            2 => hw.outbound_window(1, RP1 + 0x40_0000, 0x9000_0000, 0x10_0000),
            // A sparse BAR layout must not extend the reserved aperture.
            3 => hw.pcie.write(0x8010, 0x8080_0000),
            // No single window contains the whole function.
            _ => hw.outbound_window(0, 0x1c_0000_0000, 0x8000_0000, 0x40_0000),
        }
        let before: Vec<_> = (0x400c..0x402c)
            .step_by(4)
            .chain((0x4070..0x40a0).step_by(4))
            .map(|offset| (offset, hw.pcie.read(offset)))
            .collect();
        hw.rp1.write(0x410000, 0xdeadbeef);
        assert!(matches!(hw.discover(), Err(Error::Outbound(_))));
        assert!(!hw.dma_enabled());
        assert_eq!(hw.rp1.read(0x410000), 0xdeadbeef);
        for (offset, value) in before {
            assert_eq!(
                hw.pcie.read(offset),
                value,
                "scenario {scenario}, {offset:#x}"
            );
        }
    }
}

#[test]
fn rejected_relocation_writes_stop_before_mmio_or_dma_setup() {
    for window in 0..4 {
        for offset in [
            0x4070 + window * 4,
            0x4080 + window * 8,
            0x4084 + window * 8,
        ] {
            let hw = Hardware::new();
            hw.firmware_outbound(window);
            let forced = hw.pcie.read(offset);
            FIXED_REGISTER.with(|value| value.set(Some((offset, forced))));
            hw.rp1.write(0x410000, 0xdeadbeef);
            let dma: Vec<_> = (0..10)
                .flat_map(|index| {
                    let (bar, remap) = inbound_offsets(index);
                    [bar, bar + 4, remap, remap + 4]
                })
                .map(|offset| (offset, hw.pcie.read(offset)))
                .collect();
            let state = match hw.discover() {
                Err(Error::OutboundWrite(state)) => state,
                _ => panic!("write rejection at {offset:#x} must stop relocation"),
            };
            assert_eq!(state.register, offset);
            assert_eq!(state.observed, forced);
            assert_ne!(state.observed & state.mask, state.expected & state.mask);
            assert!(!hw.dma_enabled());
            assert_eq!(hw.rp1.read(0x410000), 0xdeadbeef);
            for (offset, value) in dma {
                assert_eq!(hw.pcie.read(offset), value);
            }
        }
    }
}

#[test]
fn outbound_window_only_needs_to_cover_the_actual_bars() {
    let hw = Hardware::new();
    // A 5 MiB window covers all three BARs, but not the unused remainder of
    // the 8 MiB early MMIO reservation. Do not require that remainder to decode.
    hw.pcie.write(0x4070, 0x0040_0000);
    let prepared = hw.prepared();
    assert_eq!(
        crate::require_some(prepared.bars[1])
            .mapping
            .resource()
            .start(),
        RP1
    );
    assert!(!hw.dma_enabled());
}

#[test]
fn inherited_bus_addresses_are_translated_by_live_outbound_windows() {
    let hw = Hardware::new();
    // DT describes the OS allocation aperture, not a snapshot of firmware's
    // programmed PCI addresses. Preserve a nonzero bus base and all BARs.
    hw.pcie.write(0x400c, 0x8000_0000);
    hw.pcie.write(0x4070, 0x0040_0000);
    for (index, offset) in [TABLE as u32, 0, 0x410000].into_iter().enumerate() {
        hw.pcie.write(0x8010 + index * 4, 0x8000_0000 + offset);
    }
    let prepared = hw.prepared();
    for (index, offset) in [TABLE as u32, 0, 0x410000].into_iter().enumerate() {
        assert_eq!(
            crate::require_some(prepared.bars[index])
                .mapping
                .resource()
                .start(),
            RP1 + u64::from(offset)
        );
        assert_eq!(hw.pcie.read(0x8010 + index * 4), 0x8000_0000 + offset);
    }
    assert_eq!(hw.pcie.read(0x400c), 0x8000_0000);
    assert!(!hw.dma_enabled());
}

#[test]
fn separate_outbound_windows_can_cover_different_bars() {
    let hw = Hardware::new();
    hw.outbound_window(0, RP1, 0xffc0_0000, 0x40_0000);
    hw.pcie.write(0x8014, 0xffc0_0000);
    hw.outbound_window(1, RP1 + 0x40_0000, 0x40_0000, 0x10_0000);
    let prepared = hw.prepared();
    assert_eq!(
        crate::require_some(prepared.bars[1])
            .mapping
            .resource()
            .start(),
        RP1
    );
    assert_eq!(
        crate::require_some(prepared.bars[0])
            .mapping
            .resource()
            .start(),
        RP1 + TABLE as u64
    );
    assert!(!hw.dma_enabled());
}

#[test]
fn outbound_translation_handles_cpu_high_bits_and_offset_within_window() {
    let hw = Hardware::new();
    // Window straddles a 4 GiB boundary. The BARs still map entirely inside
    // the aperture; its unused prefix must never become mapping authority.
    hw.outbound_window(0, RP1 - 0x40_0000, 0, 0x90_0000);
    for (index, offset) in [TABLE as u32, 0, 0x410000].into_iter().enumerate() {
        hw.pcie.write(0x8010 + index * 4, 0x40_0000 + offset);
    }
    let prepared = hw.prepared();
    assert_eq!(
        crate::require_some(prepared.bars[1])
            .mapping
            .resource()
            .start(),
        RP1
    );
    assert_eq!(
        crate::require_some(prepared.bars[0])
            .mapping
            .resource()
            .start(),
        RP1 + TABLE as u64
    );
}

#[test]
fn unsafe_outbound_translations_fail_before_function_mmio() {
    use hyper_drivers::pci::OutboundFailure;
    for scenario in 0..7 {
        let hw = Hardware::new();
        hw.rp1.write(VECTOR, 0xdeadbeef);
        let expected = match scenario {
            0 => {
                // Disabled window: base is above inclusive limit.
                hw.pcie.write(0x4070, 0x10);
                OutboundFailure::NoWindow
            }
            1 => {
                hw.outbound_window(0, RP1, 0, 0x30_0000);
                OutboundFailure::NoWindow
            }
            2 => {
                hw.outbound_window(0, RP1, 0x1000_0000, 0x80_0000);
                OutboundFailure::NoWindow
            }
            3 => {
                hw.pcie.write(0x8010, 0x80_0000);
                OutboundFailure::OutsideAperture
            }
            4 => {
                // Two windows decode the same CPU addresses, even though
                // their PCI target ranges differ.
                hw.outbound_window(1, RP1 + 0x40_0000, 0x8000_0000, 0x10_0000);
                OutboundFailure::AmbiguousTranslation
            }
            5 => {
                // Distinct CPU windows alias the same BAR in PCI space.
                hw.outbound_window(0, RP1, 0, 0x50_0000);
                hw.outbound_window(1, RP1 + 0x50_0000, 0x40_0000, 0x10_0000);
                OutboundFailure::AmbiguousTranslation
            }
            _ => {
                hw.outbound_window(0, RP1, 0xffff_ffff_fff0_0000, 0x50_0000);
                OutboundFailure::AddressOverflow
            }
        };
        let error = match hw.discover() {
            Err(Error::Outbound(state)) => state,
            _ => panic!("scenario {scenario} must reject outbound translation"),
        };
        assert_eq!(error.failure, expected, "scenario {scenario}");
        assert_eq!(error.bar, 0);
        let diagnostic = format!(
            "HypeR: PCI function handoff unavailable: {:?}",
            Error::Outbound(error)
        );
        assert!(diagnostic.contains("bus_hi:lo/base_limit/base_hi/limit_hi"));
        assert!(diagnostic.contains("cpu_aperture: 0x1f00000000"));
        assert!(diagnostic.len() <= hyper::config::LOG_LINE_MAX as usize);
        assert!(!hw.dma_enabled());
        assert_eq!(hw.rp1.read(VECTOR), 0xdeadbeef);
    }
}

#[test]
fn malformed_bars_report_the_failing_register_and_restore_decode() {
    for (value, bar_type) in [(0x400001, true), (0x400002, true), (0x401000, false)] {
        let hw = Hardware::new();
        hw.pcie.write(0x8010, value);
        let error = crate::require_some(hw.discover().err());
        if bar_type {
            assert_eq!(error, Error::BarType { index: 0, value });
        } else {
            assert_eq!(
                error,
                Error::BarSize {
                    index: 0,
                    address: u64::from(value),
                    mask: 0xffffc000
                }
            );
        }
        assert_eq!(hw.pcie.read(0x8010), value);
        assert_eq!(hw.pcie.read(COMMAND) & 7, 2);
    }
    let hw = Hardware::new();
    hw.pcie.write(0x8010, 0);
    assert!(matches!(
        hw.discover(),
        Err(Error::BarOverlap {
            first: 0,
            second: 1
        })
    ));
    assert!(!hw.dma_enabled());
}

#[test]
fn rejected_bus_routing_never_touches_endpoint_configuration() {
    let hw = Hardware::new();
    hw.pcie.write(0x18, 0);
    hw.pcie.write(0x9000, 0x1234);
    FIXED_BUS_NUMBERS.with(|value| value.set(Some(0)));
    assert!(matches!(
        hw.discover(),
        Err(Error::Link(LinkState {
            failure: LinkFailure::BusNumbersRejected,
            bus_numbers: 0,
            ..
        }))
    ));
    assert_eq!(hw.pcie.read(0x9000), 0x1234);
    assert!(hw.dma_enabled());
}

#[test]
fn discovery_gates_dma_and_msi_without_touching_function_registers() {
    let hw = Hardware::new();
    hw.rp1.write(GEM, 0xdeadbeef);
    hw.rp1.write(APBS + 0x20, 0x1234);
    hw.pcie.write(0x8000, 0x5678_1234); // No vendor-specific admission.
    let prepared = hw.prepared();
    assert_eq!(prepared.transport.identity(), 0x5678_1234);
    assert_eq!(prepared.interrupt, 160);
    assert_eq!(prepared.interrupt_count, 64);
    assert!(!hw.dma_enabled());
    assert_eq!(hw.rp1.read(GEM), 0xdeadbeef);
    assert_eq!(hw.rp1.read(APBS + 0x20), 0x1234);
    assert_eq!(hw.rp1.read(VECTOR), 0xfffff000);
    assert_eq!(hw.rp1.read(VECTOR + 4), 0xff);
    assert_eq!(hw.rp1.read(VECTOR + 8), 6);
    assert_eq!(hw.rp1.read(VECTOR + 12), 1);
    assert_eq!(hw.pcie.read(0x8010), TABLE as u32);
    assert_eq!(hw.pcie.read(0x8014), 0);
    assert_eq!(hw.pcie.read(0x8018), 0x410000);
    assert_eq!(hw.pcie.read(CAPABILITY) & 0xc0000000, 0x40000000);
}

#[test]
fn guest_config_probes_and_moves_only_shadow_bar_addresses() {
    let hw = Hardware::new();
    let prepared = hw.prepared();
    let transport = prepared.transport;
    let mut state = transport.state(GUEST, 128);
    let bar = crate::require_some(prepared.bars[0]);
    assert_eq!(transport.read(&mut state, 0x10, 4), Some(bar.offset));
    assert!(transport.write(&mut state, 0x10, 4, u64::from(u32::MAX)));
    assert_eq!(transport.read(&mut state, 0x10, 4), Some(0xffffc000));
    assert_eq!(hw.pcie.read(0x8010), TABLE as u32);
    assert!(transport.write(&mut state, 0x10, 4, 0x220000));
    assert_eq!(transport.read(&mut state, 0x10, 4), Some(0x220000));
    assert!(!transport.write(&mut state, 0x10, 4, 0x400000)); // BAR1 overlap.
    assert!(!transport.write(&mut state, 0x10, 4, 0x100000));
    assert!(transport.write(&mut state, 4, 2, 6));
    assert!(!hw.dma_enabled()); // Not published/activated yet.
    transport.activate(&mut state);
    assert!(hw.dma_enabled());
    transport.stop(&mut state);
    assert!(!hw.dma_enabled());
    assert_eq!(hw.pcie.read(0x8010), TABLE as u32);
    assert_eq!(
        transport.read(&mut state, 4096, 4),
        Some(u64::from(u32::MAX))
    );
    assert!(transport.write(&mut state, 4096, 4, 7));
    assert_eq!(transport.read(&mut state, 0x100, 4), Some(0)); // No unsafe extended caps.
}

#[test]
fn msi_table_routes_only_valid_guest_vectors_and_rewrites_physical_messages() {
    let hw = Hardware::new();
    let prepared = hw.prepared();
    let transport = prepared.transport;
    let mut state = transport.state(GUEST, 128);
    transport.activate(&mut state);
    assert!(transport.write(&mut state, 4, 2, 6));
    let table = crate::require_some(prepared.bars[0]).offset as usize + 6 * 16;
    assert!(transport.write(&mut state, table, 8, GUEST + 0x100040));
    assert!(transport.write(&mut state, table + 8, 4, 134));
    assert!(transport.write(&mut state, table + 12, 4, 0));
    assert_eq!(state.msi_irq(6), None); // Function still masked/disabled.
    assert!(transport.write(&mut state, 0x42, 2, 0x8000 | 63));
    assert_eq!(state.msi_irq(6), Some(134));
    assert_eq!(state.delivered_irq(6), Some(134));
    assert!(transport.write(&mut state, table + 12, 4, 1));
    assert_eq!(state.msi_irq(6), None);
    assert_eq!(state.delivered_irq(6), Some(134)); // An emitted edge survives masking.
    assert!(transport.write(&mut state, table + 12, 4, 0));
    assert_eq!(hw.rp1.read(VECTOR), 0xfffff000);
    assert_eq!(hw.rp1.read(VECTOR + 4), 0xff);
    assert_eq!(hw.rp1.read(VECTOR + 8), 6); // Never guest SPI134.
    assert_eq!(hw.rp1.read(VECTOR + 12), 0);
    assert!(transport.write(&mut state, table + 8, 4, 64));
    assert_eq!(state.msi_irq(6), None);
    assert_eq!(hw.rp1.read(VECTOR + 12), 1);
    assert!(transport.write(&mut state, table + 8, 4, 134));
    assert!(transport.write(&mut state, table, 8, 0xfffff000)); // Physical target injection rejected.
    assert_eq!(state.msi_irq(6), None);
    assert_eq!(hw.rp1.read(VECTOR), 0xfffff000);
    assert_eq!(hw.rp1.read(VECTOR + 4), 0xff);
    assert_eq!(hw.rp1.read(VECTOR + 12), 1);
    transport.stop(&mut state);
    assert_eq!(state.msi_irq(6), None);
    assert!(!hw.dma_enabled());
    assert_eq!(state.delivered_irq(6), None);
    assert_eq!(hw.mip.read(0x40), u32::MAX);
    assert_eq!(hw.mip.read(0x50), u32::MAX);
}

#[test]
fn vector_frame_and_opaque_bar_accesses_have_exact_bounds() {
    let hw = Hardware::new();
    let prepared = hw.prepared();
    let transport = prepared.transport;
    let mut state = transport.state(GUEST, 128);
    transport.activate(&mut state);
    assert_eq!(
        transport.read(&mut state, 0x100008, 4),
        Some((128 << 16) | 64)
    );
    assert!(transport.write(&mut state, 0x100040, 4, 191));
    assert_eq!(state.take_pending_irq(), Some(191));
    assert!(transport.write(&mut state, 0x100040, 4, 192));
    assert_eq!(state.take_pending_irq(), None);
    assert!(transport.write(&mut state, 4, 2, 2));
    let offset = crate::require_some(prepared.bars[1]).offset as usize + GEM;
    assert!(transport.write(&mut state, offset, 4, 0x12345678));
    assert_eq!(hw.rp1.read(GEM), 0x12345678);
    assert_eq!(transport.read(&mut state, offset + 1, 1), Some(0x56));
    assert_eq!(transport.read(&mut state, offset + 1, 4), None);
    assert_eq!(transport.read(&mut state, 0x800000, 4), None);
    let pba = crate::require_some(prepared.bars[0]).offset as usize + 0x800;
    hw.rp1.write(TABLE + 0x800, 0x40);
    assert_eq!(transport.read(&mut state, pba, 4), Some(0x40));
    assert!(transport.write(&mut state, pba, 4, 0));
    assert_eq!(hw.rp1.read(TABLE + 0x800), 0x40);
}

#[test]
fn missing_firmware_dma_setup_is_initialized_from_dt() {
    let hw = Hardware::new();
    for index in 0..10 {
        let (bar, remap) = inbound_offsets(index);
        for offset in [bar, bar + 4, remap, remap + 4] {
            hw.pcie.write(offset, 0);
        }
    }
    let prepared = hw.prepared();
    assert_eq!(prepared.transport.dma_offset(), 0x10_0000_0000);
    // DT order: owned MMIO loopback, RAM, then the MIP MSI page.
    for (register, expected) in [
        (0x402c, 7),
        (0x4030, 0),
        (0x40ac, 1),
        (0x40b0, 0x1f),
        (0x4034, 21),
        (0x4038, 0x10),
        (0x40b4, 1),
        (0x40b8, 0),
        (0x403c, 0xffff_f01c),
        (0x4040, 0xff),
        (0x40bc, 0x130001),
        (0x40c0, 0x10),
    ] {
        assert_eq!(hw.pcie.read(register), expected, "register {register:#x}");
    }
    assert_ne!(hw.pcie.read(0x4008) & 0x1000, 0);
    assert!(!hw.dma_enabled());
}

#[test]
fn all_inherited_dma_decoders_close_before_any_new_window_opens() {
    let hw = Hardware::new();
    for index in 0..10 {
        let (bar, remap) = inbound_offsets(index);
        hw.pcie.write(bar, 22); // Invalid size encoding must not survive.
        hw.pcie.write(remap, 0x1001); // An unvalidated target/alias.
    }
    DMA_PREVIOUS_MASK.with(|value| value.set(0x3ff));
    TRACE_DMA.with(|value| value.set(true));
    hw.pcie.write(0x4008, 0xa5a0_0200);
    let _ = hw.prepared();
    assert!(DMA_EMPTY_SEEN.with(Cell::get));
    assert!(!DMA_BAD_ORDER.with(Cell::get));
    assert_eq!(hw.pcie.read(0x4008), 0xa5a0_1200);
    for index in 3..10 {
        let (bar, remap) = inbound_offsets(index);
        assert_eq!(hw.pcie.read(bar) & 31, 0);
        assert_eq!(hw.pcie.read(remap) & 1, 0);
    }
    assert!(!hw.dma_enabled());
}

#[test]
fn rejected_dma_register_writes_keep_the_function_unpublished() {
    use hyper_drivers::pci::DmaStage;
    for (offset, forced, stage) in [
        (0x402c, 1, DmaStage::DisableWindow),
        (0x4030, 0xff, DmaStage::ProgramWindow),
        (0x40ac, 0, DmaStage::ProgramWindow),
        (0x4008, 0, DmaStage::EnableAccess),
    ] {
        let hw = Hardware::new();
        hw.rp1.write(VECTOR, 0xdeadbeef);
        FIXED_REGISTER.with(|value| value.set(Some((offset, forced))));
        let state = match hw.discover() {
            Err(Error::Dma(state)) => state,
            _ => panic!("write rejection at {offset:#x} must fail admission"),
        };
        assert_eq!(state.stage, stage);
        assert_eq!(state.bridge, PCIE);
        assert_eq!(state.register, offset);
        assert_eq!(state.observed, forced);
        assert_ne!(state.observed & state.mask, state.expected & state.mask);
        assert!(!hw.dma_enabled());
        assert_eq!(hw.rp1.read(VECTOR), 0xdeadbeef);
        let message = format!(
            "HypeR: PCI function handoff unavailable: {:?}",
            Error::Dma(state)
        );
        assert!(message.len() <= hyper::config::LOG_LINE_MAX as usize);
    }
}

#[test]
fn dma_firmware_ranges_are_validated_before_touching_the_bridge() {
    let mut malformed = vec![vec![], DMA_RANGES[..14].to_vec(), vec![0; 11 * 7]];
    for (index, value) in [
        (0, 0x0100_0000), // I/O space cannot be a DMA target.
        (1, 0x10),        // Alias the RAM bus window.
        (3, 0x11),        // Unowned SoC MMIO.
        (4, 1),           // Unaligned CPU target.
        (6, 0x5000),      // Non-power-of-two size.
        (7, 0x4200_0000), // A 32-bit PCI range cannot address 64 GiB.
        (8, 0x20),        // Wrong physical-to-device RAM offset.
        (12, 0x20),       // RAM aperture larger than this transport supports.
        (18, 0x131000),   // Wrong MSI destination.
        (20, 0x2000),     // MSI target must be exactly the admitted page.
    ] {
        let mut ranges = DMA_RANGES.to_vec();
        ranges[index] = value;
        malformed.push(ranges);
    }
    for dma in std::iter::once(None).chain(malformed.iter().map(|ranges| Some(ranges.as_slice()))) {
        let hw = Hardware::new();
        hw.pcie.write(0x9000, 0x1234);
        let nodes = firmware_with_dma(0x4000_0000, dma);
        assert!(matches!(
            Transport::discover::<TestBarrier>(&nodes, &hw),
            Err(Error::Firmware)
        ));
        assert_eq!(hw.pcie.read(0x9000), 0x1234);
        assert_eq!(hw.pcie.read(0x402c), 21);
        assert!(hw.dma_enabled()); // No device has been claimed or changed.
    }
}

#[test]
fn inbound_window_sizes_cover_the_small_encoding_discontinuity() {
    for bits in 12..=23 {
        let hw = Hardware::new();
        let mut ranges = DMA_RANGES.to_vec();
        ranges[6] = 1 << bits;
        let nodes = firmware_with_dma(0x4000_0000, Some(&ranges));
        let _ = crate::require_some(crate::require_ok(Transport::discover::<TestBarrier>(
            &nodes, &hw,
        )));
        let expected = if bits <= 15 { bits + 16 } else { bits - 15 };
        assert_eq!(hw.pcie.read(0x402c) & 31, expected);
        assert_eq!(hyper_drivers::pci::inbound_size(expected), Some(1 << bits));
    }
}

#[test]
fn malformed_msix_layout_keeps_bus_master_closed() {
    for (register, value, expected) in [
        (0x8044, 0x4000, Error::Interrupt),
        (0x8044, 7, Error::Interrupt),
        (0x8048, 0, Error::Interrupt),
    ] {
        let hw = Hardware::new();
        hw.pcie.write(register, value);
        assert!(matches!(hw.discover(), Err(error) if error == expected));
        assert!(!hw.dma_enabled());
    }
}

#[test]
fn future_imported_ram_must_fit_the_configured_dma_aperture() {
    let hw = Hardware::new();
    hw.pcie.write(0x9000, 0x1234);
    let nodes = firmware(0x10_0000_1000);
    assert!(matches!(
        Transport::discover::<TestBarrier>(&nodes, &hw),
        Err(Error::Firmware)
    ));
    assert_eq!(hw.pcie.read(0x9000), 0x1234);
}

#[test]
fn largest_first_bar_placement_uses_gaps_without_vendor_sizes() {
    use {
        hyper_drivers::pci::model::BarLayout, hyper_drivers::pci::model::MsixLayout,
        hyper_drivers::pci::model::place_bars,
    };
    let mut bars = [None; 6];
    for (index, size) in [(0, 0x4000), (1, 0x400000), (2, 0x10000)] {
        bars[index] = Some(BarLayout {
            index: index as u32,
            address: 0,
            size,
            flags: 0,
            offset: 0,
        });
    }
    assert!(place_bars(&mut bars));
    assert_eq!(crate::require_some(bars[1]).offset, 0x400000);
    assert_eq!(crate::require_some(bars[2]).offset, 0x200000);
    assert_eq!(crate::require_some(bars[0]).offset, 0x210000);
    assert!(
        MsixLayout {
            count: 64,
            table: 0,
            pending: 0x800
        }
        .valid(&bars)
    );
    assert!(
        !MsixLayout {
            count: 65,
            table: 0,
            pending: 0x800
        }
        .valid(&bars)
    );
    assert!(
        !MsixLayout {
            count: 64,
            table: 0,
            pending: 0x200
        }
        .valid(&bars)
    );
    crate::require_some(bars[0].as_mut()).size = 0x800000;
    assert!(!place_bars(&mut bars));
}

#[test]
fn legacy_msi_and_rom_decode_are_disabled_before_assignment() {
    let hw = Hardware::new();
    hw.pcie.write(CAPABILITY, (63 << 16) | (0x50 << 8) | 0x11);
    hw.pcie.write(0x8050, (1 << 16) | 0x05);
    hw.pcie.write(0x8030, 0x500001);
    hw.pcie.write(COMMAND, (1 << 20) | 7);
    let _ = hw.prepared();
    assert_eq!(hw.pcie.read(0x8050) & (1 << 16), 0);
    assert_eq!(hw.pcie.read(0x8030) & 1, 0);
    assert_eq!(hw.pcie.read(COMMAND) & 5, 0);
    assert_ne!(hw.pcie.read(COMMAND) & 0x400, 0); // Legacy INTx has no owner.
}

#[test]
fn cyclic_capability_list_is_rejected_with_dma_gated() {
    let hw = Hardware::new();
    hw.pcie.write(CAPABILITY, (63 << 16) | (0x50 << 8) | 0x11);
    hw.pcie.write(0x8050, (0x40 << 8) | 0x05);
    assert!(matches!(hw.discover(), Err(Error::Interrupt)));
    assert!(!hw.dma_enabled());
}

#[test]
fn wide_bar_probe_restores_shadow_pair_without_truncating_address_space() {
    use {
        hyper_drivers::pci::model::BarLayout, hyper_drivers::pci::model::FunctionState,
        hyper_drivers::pci::model::MsixLayout,
    };
    let mut bars = [None; 6];
    bars[0] = Some(BarLayout {
        index: 0,
        address: 0x400000,
        size: 0x4000,
        flags: 1,
        offset: 0x200000,
    });
    let mut state = FunctionState::new(
        bars,
        MsixLayout {
            count: 8,
            table: 0,
            pending: 0x800,
        },
        0x12345678,
        0,
        0,
        GUEST,
        128,
    );
    assert!(state.config_write(0x10, 4, u64::from(u32::MAX)));
    assert!(state.config_write(0x14, 4, u64::from(u32::MAX)));
    assert_eq!(state.config_read(0x10, 4), Some(0xffffc004));
    assert_eq!(state.config_read(0x14, 4), Some(u64::from(u32::MAX)));
    assert!(state.config_write(0x14, 4, 0));
    assert!(state.config_write(0x10, 4, 0x204000));
    assert_eq!(state.config_read(0x10, 4), Some(0x204004));
    assert!(!state.config_write(0x14, 4, 1));
    assert_eq!(state.config_read(0x14, 4), Some(0));
    assert_eq!(state.config_read(0x10, 4), Some(0x204004));
}
