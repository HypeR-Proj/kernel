// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Device-tree discovery, boot-property, and platform matching contracts.

use std::boxed::Box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper::{
    drivers::console,
    drivers::platform::{
        DeviceScanner, DriverInstance, DriverManager, DriverServices, MmioMappingError,
        MmioResource, PermanentMmioMapping, PlatformDevice, PlatformDriver, ProbeError, ScanError,
    },
    platform::{chosen, fdt},
};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const HEADER_SIZE: usize = 40;

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn pad(output: &mut Vec<u8>) {
    while output.len() & 3 != 0 {
        output.push(0);
    }
}

fn begin_node(structure: &mut Vec<u8>, name: &[u8]) {
    push_u32(structure, FDT_BEGIN_NODE);
    structure.extend_from_slice(name);
    structure.push(0);
    pad(structure);
}

fn property(structure: &mut Vec<u8>, name_offset: u32, value: &[u8]) {
    push_u32(structure, FDT_PROP);
    push_u32(structure, value.len() as u32);
    push_u32(structure, name_offset);
    structure.extend_from_slice(value);
    pad(structure);
}

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn qemu_like_dtb() -> Vec<u8> {
    const ADDRESS_CELLS: u32 = 0;
    const SIZE_CELLS: u32 = 15;
    const REG: u32 = 27;
    const COMPATIBLE: u32 = 31;
    const DEVICE_TYPE: u32 = 42;
    const RANGES: u32 = 54;
    const NO_MAP: u32 = 61;
    const INTERRUPTS: u32 = 68;
    const METHOD: u32 = 79;
    const STATUS: u32 = 86;
    const BOOTARGS: u32 = 93;
    const KASLR_SEED: u32 = 102;
    const INITRD_START: u32 = 113;
    const INITRD_END: u32 = 132;

    let strings = b"#address-cells\0#size-cells\0reg\0compatible\0device_type\0ranges\0no-map\0interrupts\0method\0status\0bootargs\0kaslr-seed\0linux,initrd-start\0linux,initrd-end\0";
    let mut structure = Vec::new();
    begin_node(&mut structure, b"");
    property(&mut structure, ADDRESS_CELLS, &2u32.to_be_bytes());
    property(&mut structure, SIZE_CELLS, &2u32.to_be_bytes());
    begin_node(&mut structure, b"chosen");
    property(
        &mut structure,
        BOOTARGS,
        b"earlycon=pl011,mmio32,0x09000000 loglevel=7\0",
    );
    property(
        &mut structure,
        KASLR_SEED,
        &0x0123_4567_89ab_cdef_u64.to_be_bytes(),
    );
    property(
        &mut structure,
        INITRD_START,
        &0x0000_0000_4800_0000_u64.to_be_bytes(),
    );
    property(
        &mut structure,
        INITRD_END,
        &0x0000_0000_4900_0000_u64.to_be_bytes(),
    );
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"memory@40000000");
    property(&mut structure, DEVICE_TYPE, b"memory\0");
    property(
        &mut structure,
        REG,
        &[0, 0, 0, 0, 0x40, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0],
    );
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"cpus");
    property(&mut structure, ADDRESS_CELLS, &2u32.to_be_bytes());
    property(&mut structure, SIZE_CELLS, &0u32.to_be_bytes());
    for hardware_id in 0..5u32 {
        begin_node(&mut structure, b"cpu");
        property(&mut structure, DEVICE_TYPE, b"cpu\0");
        property(&mut structure, REG, &cells(&[0, hardware_id]));
        if hardware_id == 4 {
            property(&mut structure, STATUS, b"fail\0");
        }
        push_u32(&mut structure, FDT_END_NODE);
    }
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"reserved-memory");
    property(&mut structure, RANGES, &[]);
    property(&mut structure, ADDRESS_CELLS, &2u32.to_be_bytes());
    property(&mut structure, SIZE_CELLS, &2u32.to_be_bytes());
    begin_node(&mut structure, b"firmware@41000000");
    property(
        &mut structure,
        REG,
        &[0, 0, 0, 0, 0x41, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0],
    );
    property(&mut structure, NO_MAP, &[]);
    push_u32(&mut structure, FDT_END_NODE);
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"intc@8000000");
    property(
        &mut structure,
        REG,
        &cells(&[
            0,
            0x0800_0000,
            0,
            0x0001_0000,
            0,
            0x080a_0000,
            0,
            0x00f6_0000,
        ]),
    );
    property(&mut structure, COMPATIBLE, b"arm,gic-v3\0");
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"timer");
    property(
        &mut structure,
        COMPATIBLE,
        b"arm,armv8-timer\0arm,armv7-timer\0",
    );
    property(
        &mut structure,
        INTERRUPTS,
        &cells(&[1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]),
    );
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"psci");
    property(
        &mut structure,
        COMPATIBLE,
        b"arm,psci-1.0\0arm,psci-0.2\0arm,psci\0",
    );
    property(&mut structure, METHOD, b"smc\0");
    push_u32(&mut structure, FDT_END_NODE);
    begin_node(&mut structure, b"soc");
    property(
        &mut structure,
        RANGES,
        &[0, 0, 0, 0, 0, 0, 0, 0, 0x09, 0, 0, 0, 0, 0x10, 0, 0],
    );
    property(&mut structure, ADDRESS_CELLS, &1u32.to_be_bytes());
    property(&mut structure, SIZE_CELLS, &1u32.to_be_bytes());
    begin_node(&mut structure, b"pl011@0");
    property(&mut structure, REG, &[0, 0, 0, 0, 0, 0, 0x10, 0]);
    property(&mut structure, COMPATIBLE, b"arm,pl011\0arm,primecell\0");
    push_u32(&mut structure, FDT_END_NODE);
    push_u32(&mut structure, FDT_END_NODE);
    push_u32(&mut structure, FDT_END_NODE);
    push_u32(&mut structure, FDT_END);

    let reservation_offset = HEADER_SIZE;
    let structure_offset = reservation_offset + 16;
    let strings_offset = structure_offset + structure.len();
    let total_size = strings_offset + strings.len();
    let mut blob = Vec::new();
    for value in [
        FDT_MAGIC,
        total_size as u32,
        structure_offset as u32,
        strings_offset as u32,
        reservation_offset as u32,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ] {
        push_u32(&mut blob, value);
    }
    blob.extend_from_slice(&[0; 16]);
    blob.extend_from_slice(&structure);
    blob.extend_from_slice(strings);
    blob
}

fn replace_first(blob: &mut [u8], old: &[u8], new: &[u8]) {
    assert_eq!(old.len(), new.len());
    let index = crate::require_some(blob.windows(old.len()).position(|window| window == old));
    blob[index..index + new.len()].copy_from_slice(new);
}

#[test]
fn discovers_generic_devices_and_translates_resources() {
    let blob = qemu_like_dtb();
    let mut scanner = DeviceScanner::new(&[]);
    let mut chosen = chosen::Discovery::new();
    let platform = {
        let mut visitors = fdt::VisitorPair::new(&mut scanner, &mut chosen);
        crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut visitors))
    };
    let devices = crate::require_ok(scanner.finish());
    let chosen = crate::require_ok(chosen.finish());
    let command_line = crate::require_some(chosen.command_line());
    assert_eq!(command_line.value("loglevel"), Some("7"));
    assert_eq!(chosen.kaslr_seed(), Some(0x0123_4567_89ab_cdef));
    assert_eq!(chosen.command_line_error(), None);
    assert_eq!(chosen.kaslr_seed_error(), None);
    let initrd = crate::require_some(chosen.initial_ramdisk());
    assert_eq!(initrd.start(), 0x4800_0000);
    assert_eq!(initrd.end(), 0x4900_0000);
    assert_eq!(
        crate::require_ok(console::early_console(Some(command_line))),
        Some(hyper::platform::ConsoleInfo {
            kind: hyper::platform::ConsoleKind::Pl011,
            base: 0x0900_0000,
            access: hyper::platform::ConsoleRegisterAccess::Native,
        })
    );

    let console = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,pl011")),
    );
    assert_eq!(console.registers()[0].start(), 0x0900_0000);
    let gic = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,gic-v3")),
    );
    assert_eq!(gic.registers()[0].start(), 0x0800_0000);
    assert_eq!(gic.registers()[1].start(), 0x080a_0000);
    let timer = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,armv8-timer")),
    );
    assert_eq!(&timer.interrupt_cells()[9..12], &[1, 10, 4]);
    let psci = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,psci-1.0")),
    );
    assert_eq!(psci.property("method"), Some(b"smc\0".as_slice()));
    assert_eq!(
        platform.memory.as_slice(),
        &[crate::require_some(hyper::platform::PhysicalRange::new(
            0x4000_0000,
            0x2000_0000,
        ))]
    );
    assert_eq!(
        platform
            .cpus
            .as_slice()
            .iter()
            .map(|cpu| cpu.hardware_id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(
        platform
            .mmio
            .as_slice()
            .iter()
            .any(|range| range.start() <= 0x0900_0000 && 0x0900_1000 <= range.end())
    );
    assert_eq!(
        platform.no_map.as_slice(),
        &[crate::require_some(hyper::platform::PhysicalRange::new(
            0x4100_0000,
            0x2000,
        ))]
    );
}

#[test]
fn preserves_the_initrd_when_optional_bootargs_are_invalid() {
    let mut blob = qemu_like_dtb();
    let old = b"earlycon=pl011,mmio32,0x09000000 loglevel=7\0";
    let mut invalid = *old;
    invalid[0] = 0xff;
    replace_first(&mut blob, old, &invalid);

    let mut chosen = chosen::Discovery::new();
    let _ = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut chosen));
    let chosen = crate::require_ok(chosen.finish());
    assert!(chosen.command_line().is_none());
    assert_eq!(
        chosen.command_line_error(),
        Some(chosen::Error::InvalidEncoding)
    );
    let initrd = crate::require_some(chosen.initial_ramdisk());
    assert_eq!(initrd.start(), 0x4800_0000);
    assert_eq!(initrd.end(), 0x4900_0000);
}

#[test]
fn rejects_embedded_nuls_in_command_lines_from_non_fdt_boot_protocols() {
    assert_eq!(
        chosen::CommandLine::parse("console=ttyS0\0init=/bin/sh").map(|_| ()),
        Err(chosen::Error::InvalidEncoding)
    );
}

#[test]
fn leaves_the_early_console_disabled_without_an_earlycon_argument() {
    let mut blob = qemu_like_dtb();
    replace_first(&mut blob, b"earlycon", b"consolex");
    let mut discovery = chosen::Discovery::new();
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut discovery));
    let chosen = crate::require_ok(discovery.finish());
    let command_line = crate::require_some(chosen.command_line());

    assert_eq!(
        crate::require_ok(console::early_console(Some(command_line))),
        None
    );
}

#[test]
fn rejects_an_invalid_early_console_address_without_rejecting_the_dtb() {
    let mut blob = qemu_like_dtb();
    replace_first(&mut blob, b"0x09000000", b"not-an-hex");
    let mut discovery = chosen::Discovery::new();
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut discovery));
    let chosen = crate::require_ok(discovery.finish());
    let command_line = crate::require_some(chosen.command_line());

    assert_eq!(
        console::early_console(Some(command_line)),
        Err(console::EarlyConsoleError::InvalidAddress)
    );
}

#[test]
fn applies_alignment_for_the_selected_early_console_access_width() {
    let byte_arguments = crate::require_ok(chosen::CommandLine::parse(
        "earlycon=uart8250,mmio,0x10000001",
    ));
    let byte_console = crate::require_some(crate::require_ok(console::early_console(Some(
        &byte_arguments,
    ))));
    assert_eq!(byte_console.base, 0x1000_0001);

    let word_arguments = crate::require_ok(chosen::CommandLine::parse(
        "earlycon=uart8250,mmio32,0x10000001",
    ));
    assert_eq!(
        console::early_console(Some(&word_arguments)),
        Err(console::EarlyConsoleError::InvalidAddress)
    );
}

#[test]
fn admits_only_available_device_tree_status_values() {
    let mut blob = qemu_like_dtb();
    replace_first(&mut blob, b"fail\0", b"okay\0");

    let platform = crate::require_ok(fdt::discover_from_bytes(&blob));
    assert_eq!(platform.cpus.len(), 5);

    replace_first(&mut blob, b"okay\0", b"nope\0");
    let platform = crate::require_ok(fdt::discover_from_bytes(&blob));
    assert_eq!(platform.cpus.len(), 4);

    replace_first(&mut blob, b"nope\0", b"ok\0xx");
    let platform = crate::require_ok(fdt::discover_from_bytes(&blob));
    assert_eq!(platform.cpus.len(), 4);
}

#[test]
fn rejects_a_bad_magic_number() {
    let mut blob = qemu_like_dtb();
    blob[0] = 0;

    assert!(matches!(
        fdt::discover_from_bytes(&blob),
        Err(fdt::Error::BadMagic)
    ));
}

struct RejectBeginNode;

impl fdt::NodeVisitor for RejectBeginNode {
    type Error = ();

    fn begin_node(&mut self, _id: fdt::NodeId, _name: &str) -> Result<(), Self::Error> {
        Err(())
    }
}

#[test]
fn reports_malformed_node_padding_before_a_visitor_error() {
    let mut blob = qemu_like_dtb();
    // The root node token and empty name require eight structure bytes after
    // alignment. Restricting the structure block to six makes that event
    // structurally invalid before it can be published to the visitor.
    blob[36..40].copy_from_slice(&6u32.to_be_bytes());

    assert!(matches!(
        fdt::discover_from_bytes_with(&blob, &mut RejectBeginNode),
        Err(fdt::WalkError::Fdt(fdt::Error::Truncated))
    ));
}

#[test]
fn reports_a_malformed_compatible_list_as_a_scanner_error() {
    let mut blob = qemu_like_dtb();
    let old = b"arm,pl011\0arm,primecell\0";
    let mut malformed = *old;
    let last = malformed.len() - 1;
    malformed[last] = b'x';
    replace_first(&mut blob, old, &malformed);

    let mut scanner = DeviceScanner::new(&[]);
    assert!(matches!(
        fdt::discover_from_bytes_with(&blob, &mut scanner),
        Err(fdt::WalkError::Visitor(ScanError::MalformedProperty))
    ));
}

#[test]
fn distinguishes_invalid_compatible_utf8_from_structural_errors() {
    let mut blob = qemu_like_dtb();
    let old = b"arm,pl011\0arm,primecell\0";
    let mut malformed = *old;
    malformed[0] = 0xff;
    replace_first(&mut blob, old, &malformed);

    let mut scanner = DeviceScanner::new(&[]);
    assert!(matches!(
        fdt::discover_from_bytes_with(&blob, &mut scanner),
        Err(fdt::WalkError::Visitor(ScanError::InvalidUtf8))
    ));
}

#[test]
fn reports_misaligned_interrupt_cells_as_an_fdt_error() {
    let mut blob = qemu_like_dtb();
    let descriptor = cells(&[1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]);
    let value = crate::require_some(
        blob.windows(descriptor.len())
            .position(|window| window == descriptor),
    );
    blob[value - 8..value - 4].copy_from_slice(&47u32.to_be_bytes());

    let mut scanner = DeviceScanner::new(&[]);
    assert!(matches!(
        fdt::discover_from_bytes_with(&blob, &mut scanner),
        Err(fdt::WalkError::Fdt(fdt::Error::Truncated))
    ));
}

#[test]
fn discovers_the_hvc_psci_conduit() {
    let mut blob = qemu_like_dtb();
    replace_first(&mut blob, b"smc\0", b"hvc\0");

    let mut scanner = DeviceScanner::new(&[]);
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let devices = crate::require_ok(scanner.finish());
    let psci = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,psci-1.0")),
    );
    assert_eq!(psci.property("method"), Some(b"hvc\0".as_slice()));
}

#[test]
fn leaves_legacy_psci_policy_to_the_architecture() {
    let mut blob = qemu_like_dtb();
    replace_first(&mut blob, b"arm,psci-1.0", b"old,psci-1.0");
    replace_first(&mut blob, b"arm,psci-0.2", b"old,psci-0.2");

    let mut scanner = DeviceScanner::new(&[]);
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let devices = crate::require_ok(scanner.finish());
    assert!(
        devices
            .iter()
            .any(|device| device.is_compatible("arm,psci"))
    );
}

#[test]
fn rejects_a_dtb_larger_than_the_linux_boot_limit() {
    let mut blob = qemu_like_dtb();
    blob[4..8].copy_from_slice(&(2_097_153u32).to_be_bytes());

    assert!(matches!(
        fdt::discover_from_bytes(&blob),
        Err(fdt::Error::TooLarge)
    ));
}

struct TestServices;

impl DriverServices for TestServices {
    fn map_mmio(&self, resource: MmioResource) -> Result<PermanentMmioMapping, MmioMappingError> {
        let virtual_start =
            usize::try_from(resource.start()).map_err(|_| MmioMappingError::AddressOverflow)?;
        // SAFETY: Host tests never dereference this identity-shaped mapping;
        // they exercise only driver-framework resource propagation.
        unsafe {
            PermanentMmioMapping::new(
                resource,
                hyper::mm::VirtualAddress::new(virtual_start as u64),
            )
        }
    }
}

struct TestInstance;

impl DriverInstance for TestInstance {}

struct TestDriver;

impl PlatformDriver for TestDriver {
    fn name(&self) -> &'static str {
        "test-pl011"
    }

    fn compatible_table(&self) -> &'static [&'static str] {
        &["arm,pl011"]
    }

    fn probe(
        &self,
        device: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError> {
        let register = device.registers().first().ok_or(ProbeError::Resource)?;
        let _mapping = services
            .map_mmio(*register)
            .map_err(|_| ProbeError::Resource)?;
        Ok(Box::new(TestInstance))
    }
}

static TEST_DRIVER: TestDriver = TestDriver;

struct DeferredDriver;

static DEFERRED_PROBES: AtomicUsize = AtomicUsize::new(0);

impl PlatformDriver for DeferredDriver {
    fn name(&self) -> &'static str {
        "deferred-pl011"
    }

    fn compatible_table(&self) -> &'static [&'static str] {
        &["arm,pl011"]
    }

    fn probe(
        &self,
        _device: &PlatformDevice,
        _services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError> {
        if DEFERRED_PROBES.fetch_add(1, Ordering::Relaxed) == 0 {
            Err(ProbeError::Deferred)
        } else {
            Ok(Box::new(TestInstance))
        }
    }
}

static DEFERRED_DRIVER: DeferredDriver = DeferredDriver;

static LIFECYCLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static LIFECYCLE_ACTIVATIONS: AtomicUsize = AtomicUsize::new(0);
static LIFECYCLE_REMOVALS: AtomicUsize = AtomicUsize::new(0);

struct LifecycleInstance;

impl DriverInstance for LifecycleInstance {
    fn activate(&mut self) -> Result<(), ProbeError> {
        LIFECYCLE_ACTIVATIONS.fetch_add(1, Ordering::Relaxed);
        LIFECYCLE_ACTIVE.store(true, Ordering::Release);
        Ok(())
    }

    fn remove(&mut self) -> Result<(), ProbeError> {
        LIFECYCLE_REMOVALS.fetch_add(1, Ordering::Relaxed);
        LIFECYCLE_ACTIVE.store(false, Ordering::Release);
        Ok(())
    }
}

struct LifecycleDriver;

impl PlatformDriver for LifecycleDriver {
    fn name(&self) -> &'static str {
        "lifecycle-pl011"
    }

    fn compatible_table(&self) -> &'static [&'static str] {
        &["arm,pl011"]
    }

    fn probe(
        &self,
        _device: &PlatformDevice,
        _services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError> {
        assert!(!LIFECYCLE_ACTIVE.load(Ordering::Acquire));
        Ok(Box::new(LifecycleInstance))
    }
}

static LIFECYCLE_DRIVER: LifecycleDriver = LifecycleDriver;

#[test]
fn matches_and_binds_a_registered_platform_driver() {
    let blob = qemu_like_dtb();
    let mut scanner = DeviceScanner::new(&[]);
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let devices = crate::require_ok(scanner.finish());
    let console = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,pl011")),
    );
    let mut manager = DriverManager::new();
    crate::require_ok(manager.register(&TEST_DRIVER));
    let report = manager.probe_devices(&devices, &TestServices);

    assert_eq!(report.bound, 1);
    assert_eq!(manager.binding_driver(console.id()), Some("test-pl011"));
    assert!(crate::require_ok(manager.remove(console.id())));
}

#[test]
fn retries_deferred_platform_devices_until_probe_progress_stops() {
    DEFERRED_PROBES.store(0, Ordering::Relaxed);
    let blob = qemu_like_dtb();
    let mut scanner = DeviceScanner::new(&[]);
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let devices = crate::require_ok(scanner.finish());
    let console = crate::require_some(
        devices
            .iter()
            .find(|device| device.is_compatible("arm,pl011")),
    );
    let mut manager = DriverManager::new();
    crate::require_ok(manager.register(&DEFERRED_DRIVER));

    let report = manager.probe_devices(&devices, &TestServices);

    assert_eq!(report.bound, 1);
    assert_eq!(report.deferred, 0);
    assert_eq!(DEFERRED_PROBES.load(Ordering::Relaxed), 2);
    assert_eq!(manager.binding_driver(console.id()), Some("deferred-pl011"));
    crate::require_ok(manager.retire_all());
}

#[test]
fn owns_driver_before_activation_and_retires_it_before_manager_drop() {
    LIFECYCLE_ACTIVE.store(false, Ordering::Release);
    LIFECYCLE_ACTIVATIONS.store(0, Ordering::Relaxed);
    LIFECYCLE_REMOVALS.store(0, Ordering::Relaxed);
    let blob = qemu_like_dtb();
    let mut scanner = DeviceScanner::new(&[]);
    let _platform = crate::require_ok(fdt::discover_from_bytes_with(&blob, &mut scanner));
    let devices = crate::require_ok(scanner.finish());

    {
        let mut manager = DriverManager::new();
        crate::require_ok(manager.register(&LIFECYCLE_DRIVER));
        let report = manager.probe_devices(&devices, &TestServices);
        assert_eq!(report.bound, 1);
        assert!(LIFECYCLE_ACTIVE.load(Ordering::Acquire));
        assert_eq!(LIFECYCLE_ACTIVATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(LIFECYCLE_REMOVALS.load(Ordering::Relaxed), 0);
        crate::require_ok(manager.retire_all());
    }

    assert!(!LIFECYCLE_ACTIVE.load(Ordering::Acquire));
    assert_eq!(LIFECYCLE_REMOVALS.load(Ordering::Relaxed), 1);
}
