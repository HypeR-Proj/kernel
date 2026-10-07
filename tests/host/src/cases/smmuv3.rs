// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fault injection complements real DMA tests; this mock does not translate DMA.

use crate::{require_ok, require_some};
use hyper::drivers::{
    iommu::smmuv3::{
        Capabilities, CommandError, Containment, Controller, DmaMemory, Environment, Error, Event,
        FaultKind, FaultOutcome, Permissions, firmware::pci_stream_id,
    },
    platform::{MmioResource, PermanentMmioMapping},
};
use hyper::{mm::VirtualAddress, platform::PhysicalRange};
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    cell::{RefCell, UnsafeCell},
};

#[derive(Default)]
struct State {
    registers: usize,
    time: u64,
    live_pages: usize,
    hold_update: bool,
    hold_commands: bool,
    command_error: bool,
    hold_disable: bool,
    hold_irq: bool,
    writes: Vec<(usize, u32)>,
    previous_control: u32,
    previous_ack: u32,
    observed_producer: u32,
    commands: Vec<u8>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
struct Memory {
    address: usize,
    order: usize,
}
struct TestEnvironment;

// SAFETY: Each buffer is uniquely allocated with physical-size alignment. The
// mock uses the same stable allocation for its CPU and synthetic DMA addresses.
unsafe impl DmaMemory for Memory {
    fn physical(&self) -> u64 {
        self.address as u64
    }
    fn virtual_address(&self) -> usize {
        self.address
    }
    fn order(&self) -> usize {
        self.order
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        let size = 4096 << self.order;
        let layout = require_ok(Layout::from_size_align(size, size));
        // SAFETY: Exact layout and unique ownership of this live allocation.
        unsafe { dealloc(self.address as *mut u8, layout) };
        STATE.with(|state| state.borrow_mut().live_pages -= 1 << self.order);
    }
}
impl Environment for TestEnvironment {
    type Memory = Memory;
    fn allocate(order: usize) -> Result<Memory, Error> {
        let size = 4096 << order;
        let layout = Layout::from_size_align(size, size).map_err(|_| Error::Allocation)?;
        // SAFETY: Nonempty validated allocation layout.
        let address = unsafe { alloc_zeroed(layout) } as usize;
        if address == 0 {
            return Err(Error::Allocation);
        }
        STATE.with(|state| state.borrow_mut().live_pages += 1 << order);
        Ok(Memory { address, order })
    }
    fn now_microseconds() -> u64 {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.time += 10_000;
            state.time
        })
    }
    fn synchronize() {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let base = state.registers;
            if base == 0 {
                return;
            }
            let read = |offset| {
                // SAFETY: Fixture keeps the mock register allocation live;
                // every call uses aligned in-bounds register offsets.
                unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
            };
            let write = |offset, value| {
                // SAFETY: Same fixture-owned mock MMIO allocation as read.
                unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) };
            };
            if !state.hold_update {
                write(0x44, read(0x44) & !(1 << 31));
            }
            if !state.hold_disable || read(0x20) != 0 {
                write(0x24, read(0x20));
            }
            if !state.hold_irq {
                write(0x54, read(0x50));
            }
            let control = read(0x20);
            if control != state.previous_control {
                state.writes.push((0x20, control));
                state.previous_control = control;
            }
            let ack = read(0x64);
            if ack != state.previous_ack {
                state.writes.push((0x64, ack));
                state.previous_ack = ack;
            }
            let producer = read(0x98);
            if producer != state.observed_producer {
                let base = u64::from(read(0x90)) | (u64::from(read(0x94)) << 32);
                let index = producer.wrapping_sub(1) & 255;
                let address = (base & 0x0000_ffff_ffff_f000) as usize + index as usize * 16;
                // SAFETY: The mock queue uses live host allocations as synthetic
                // physical addresses. Tests advertise 256 entries; the driver
                // publishes one initialized command before advancing PROD.
                let opcode = unsafe { core::ptr::read_volatile(address as *const u64) } as u8;
                state.commands.push(opcode);
                state.observed_producer = producer;
            }
            if state.command_error && read(0x98) != read(0x9c) {
                write(0x60, 1);
            }
            if !state.hold_commands && !state.command_error {
                write(0x9c, read(0x98));
            }
        });
    }
}

struct Fixture {
    registers: Box<[UnsafeCell<u64>]>,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            registers: (0..0x20000 / 8).map(|_| UnsafeCell::new(0)).collect(),
        };
        STATE.with(|state| {
            *state.borrow_mut() = State {
                registers: fixture.registers.as_ptr() as usize,
                ..State::default()
            }
        });
        fixture.write(0, 0x19);
        fixture.write(4, (8 << 21) | (7 << 16) | 6);
        fixture.write(0x14, (1 << 4) | 5);
        fixture
    }
    fn write(&self, offset: usize, value: u32) {
        // SAFETY: Each test uses aligned in-bounds mock registers. There is no
        // actual concurrent device, and no shared references to these bytes.
        unsafe {
            core::ptr::write_volatile(
                (self.registers.as_ptr() as usize + offset) as *mut u32,
                value,
            )
        };
    }
    fn read(&self, offset: usize) -> u32 {
        // SAFETY: Tests use aligned offsets in fixture-owned register storage.
        unsafe {
            core::ptr::read_volatile((self.registers.as_ptr() as usize + offset) as *const u32)
        }
    }
    fn initialize(&self) -> Result<Controller<TestEnvironment>, Error> {
        let range = require_some(PhysicalRange::new(0x9050000, 0x20000));
        // SAFETY: Fixture stands in for a uniquely owned, coherent, cold SMMU.
        // Its register storage outlives controller use in each test.
        unsafe {
            let mapping = require_ok(PermanentMmioMapping::new(
                MmioResource::from_physical_range(range),
                VirtualAddress::new(self.registers.as_ptr() as u64),
            ));
            Controller::initialize(mapping)
        }
    }
}

#[test]
fn rejects_unsupported_hardware_without_publication() {
    let fixture = Fixture::new();
    fixture.write(0, 0x09); // incoherent walks
    assert!(matches!(fixture.initialize(), Err(Error::Unsupported)));
    assert_eq!(STATE.with(|state| state.borrow().live_pages), 0);
    fixture.write(0, 0x19);
    fixture.write(0x20, 1);
    assert!(matches!(fixture.initialize(), Err(Error::AlreadyEnabled)));
}

#[test]
fn failed_preparation_releases_unpublished_allocations() {
    let fixture = Fixture::new();
    STATE.with(|state| state.borrow_mut().hold_update = true);
    assert!(matches!(
        fixture.initialize(),
        Err(Error::Timeout { register: 0x44, .. })
    ));
    assert_eq!(STATE.with(|state| state.borrow().live_pages), 0);
}

#[test]
fn command_timeout_quarantines_live_translation_backing() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let domain = require_ok(controller.create_domain());
    let page = require_ok(TestEnvironment::allocate(0));
    require_ok(controller.map_page(domain, 0x2000, page, Permissions::ReadWrite));
    require_ok(controller.attach(8, domain));
    let live = STATE.with(|state| state.borrow().live_pages);
    STATE.with(|state| state.borrow_mut().hold_commands = true);
    assert!(matches!(
        controller.unmap_page(domain, 0x2000),
        Err(Error::Timeout { register: 0x9c, .. })
    ));
    assert_eq!(controller.detach(8), Err(Error::Failed));
    drop(controller);
    assert_eq!(STATE.with(|state| state.borrow().live_pages), live);
}

#[test]
fn stream_publication_timeout_keeps_valid_clear() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let domain = require_ok(controller.create_domain());
    STATE.with(|state| state.borrow_mut().hold_commands = true);
    assert!(matches!(
        controller.attach(8, domain),
        Err(Error::Timeout { .. })
    ));
    // SAFETY: The fixture's STRTAB_BASE points to still-retained owned memory.
    // This test observes V after the failed prepare/invalidate phase.
    let valid = unsafe {
        let base =
            core::ptr::read_volatile((fixture.registers.as_ptr() as usize + 0x80) as *const u64);
        core::ptr::read_volatile(((base & 0x0000_ffff_ffff_f000) as usize + 8 * 64) as *const u64)
            & 1
    };
    assert_eq!(valid, 0);
}

#[test]
fn detach_completes_configuration_invalidation_before_tlb_invalidation() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let domain = require_ok(controller.create_domain());
    require_ok(controller.attach(8, domain));
    STATE.with(|state| state.borrow_mut().commands.clear());
    require_ok(controller.detach(8));
    STATE.with(|state| assert_eq!(state.borrow().commands, [0x03, 0x46, 0x28, 0x46]));
    require_ok(controller.destroy_domain(domain));
}

#[test]
fn global_error_during_publication_retains_queues() {
    let fixture = Fixture::new();
    STATE.with(|state| state.borrow_mut().command_error = true);
    assert!(matches!(fixture.initialize(), Err(Error::Global(1))));
    assert_eq!(STATE.with(|state| state.borrow().live_pages), 3);
}

#[test]
fn overflow_fails_closed_instead_of_silently_losing_faults() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    fixture.write(0x100a8, 1 << 31);
    assert_eq!(controller.next_event(), Err(Error::EventOverflow));
    assert_eq!(controller.create_domain(), Err(Error::Failed));
}

#[test]
fn event_overflow_blocks_operations_without_an_event_poll() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    fixture.write(0x100a8, 1 << 31);
    assert_eq!(controller.create_domain(), Err(Error::EventOverflow));
    assert_eq!(
        require_some(controller.failure()).containment,
        Containment::AbortAcknowledged
    );
}

#[test]
fn reattachment_waits_for_old_fault_records_to_be_handled() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let old = require_ok(controller.create_domain());
    let new = require_ok(controller.create_domain());
    require_ok(controller.attach(8, old));
    // A terminated fault was recorded before detach completed. Its SID carries
    // no software generation and must not become attributable to the new owner.
    let event = translation_fault(8);
    let base = u64::from(fixture.read(0xa0)) | (u64::from(fixture.read(0xa4)) << 32);
    let queue = (base & 0x0000_ffff_ffff_f000) as usize;
    for (word, value) in event.words.into_iter().enumerate() {
        // SAFETY: The empty mock EventQ owns this first, in-bounds record.
        unsafe { core::ptr::write_volatile((queue + word * 8) as *mut u64, value.to_le()) };
    }
    fixture.write(0x100a8, 1);
    require_ok(controller.detach(8));
    assert_eq!(controller.attach(8, new), Err(Error::FaultsPending));
    let event = require_some(require_ok(controller.next_event()));
    assert_eq!(
        require_ok(controller.quarantine_fault(event)),
        FaultOutcome::Quarantined { domain: None }
    );
    assert_eq!(controller.attach(8, new), Err(Error::StreamQuarantined));
    require_ok(controller.destroy_domain(old));
    // An unrelated stream can be bound once pending evidence is handled.
    require_ok(controller.attach(16, new));
}

#[test]
fn retiring_domain_frees_tables_and_rejects_stale_identity() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let old = require_ok(controller.create_domain());
    let page = require_ok(TestEnvironment::allocate(0));
    require_ok(controller.map_page(old, 0x2000, page, Permissions::Read));
    assert_eq!(controller.destroy_domain(old), Err(Error::DomainBusy));
    drop(require_ok(controller.unmap_page(old, 0x2000)));
    require_ok(controller.destroy_domain(old));
    assert_eq!(STATE.with(|state| state.borrow().live_pages), 3);
    let new = require_ok(controller.create_domain());
    assert_ne!(old, new);
    assert_eq!(controller.attach(8, old), Err(Error::InvalidDomain));
    require_ok(controller.destroy_domain(new));
}

#[test]
fn capability_admission_rejects_forced_stall_and_preset_tables() {
    let supported = (0x19, (8 << 21) | (7 << 16) | 16, 0x15);
    assert!(Capabilities::decode(supported.0, supported.1, supported.2).is_ok());
    for unsupported in [1 << 29, 1 << 30] {
        assert!(Capabilities::decode(supported.0, supported.1 | unsupported, supported.2).is_err());
    }
    assert!(Capabilities::decode(supported.0 | (2 << 24), supported.1, supported.2).is_err());
    assert!(Capabilities::decode(supported.0, supported.1, 0x12).is_ok());
    assert!(Capabilities::decode(supported.0, supported.1, 0x10).is_err());
}

#[test]
fn pci_routing_checks_masks_target_ambiguity_and_overflow() {
    let map = |words: &[u32]| {
        words
            .iter()
            .flat_map(|word| word.to_be_bytes())
            .collect::<Vec<_>>()
    };
    let identity = map(&[0, 42, 0x10000, 65536]);
    assert_eq!(pci_stream_id(&identity, u32::MAX, 0x1234, 42), Ok(0x11234));
    assert_eq!(pci_stream_id(&identity, 0xfff8, 0x1237, 42), Ok(0x11230));
    assert!(pci_stream_id(&identity, u32::MAX, 8, 99).is_err());
    assert!(pci_stream_id(&identity[..15], u32::MAX, 8, 42).is_err());
    assert!(pci_stream_id(&map(&[0, 42, 0, 16, 8, 42, 128, 16]), u32::MAX, 8, 42).is_err());
    assert!(pci_stream_id(&map(&[0, 42, u32::MAX, 2]), u32::MAX, 0, 42).is_err());
    assert!(pci_stream_id(&map(&[65535, 42, 0, 2]), u32::MAX, 65535, 42).is_err());
    assert!(pci_stream_id(&map(&[0, 42, 0, 0]), u32::MAX, 0, 42).is_err());
}

fn translation_fault(stream: u32) -> Event {
    Event {
        words: [0x10 | (u64::from(stream) << 32), 1 << 39, 0x3000, 0],
    }
}

#[test]
fn quarantine_preserves_binding_and_blocks_reassignment_without_stopping_other_streams() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let a = require_ok(controller.create_domain());
    let b = require_ok(controller.create_domain());
    require_ok(controller.attach(8, a));
    require_ok(controller.attach(16, b));
    STATE.with(|state| state.borrow_mut().commands.clear());
    assert_eq!(
        require_ok(controller.quarantine_fault(translation_fault(8))),
        FaultOutcome::Quarantined { domain: Some(a) }
    );
    STATE.with(|state| assert_eq!(state.borrow().commands, [3, 0x46, 3, 0x46, 0x28, 0x46]));
    assert!(controller.stream_quarantined(8));
    assert!(!controller.stream_quarantined(16));
    assert_eq!(controller.detach(8), Err(Error::StreamQuarantined));
    assert_eq!(controller.attach(8, b), Err(Error::StreamQuarantined));
    assert_eq!(controller.destroy_domain(a), Err(Error::DomainBusy));
    assert_eq!(
        require_ok(controller.quarantine_fault(translation_fault(8))),
        FaultOutcome::AlreadyQuarantined
    );
    require_ok(controller.detach(16));
    require_ok(controller.destroy_domain(b));
    assert!(controller.failure().is_none());
}

#[test]
fn fault_on_unbound_stream_gets_an_abort_entry_and_no_new_domain() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let live = STATE.with(|state| state.borrow().live_pages);
    assert_eq!(
        require_ok(controller.quarantine_fault(translation_fault(32))),
        FaultOutcome::Quarantined { domain: None }
    );
    assert_eq!(STATE.with(|state| state.borrow().live_pages), live);
    let base = u64::from(fixture.read(0x80)) | (u64::from(fixture.read(0x84)) << 32);
    // SAFETY: Live retained stream-table entry selected inside this fixture's 64 SIDs.
    let ste = unsafe {
        core::ptr::read_volatile(((base & 0x0000_ffff_ffff_f000) as usize + 32 * 64) as *const u64)
    };
    assert_eq!(ste, 1); // V=1, CFG=0: terminate without further events
}

#[test]
fn command_errors_preserve_syndrome_and_disable_before_acknowledging() {
    for code in [1, 2, 3, 127] {
        let fixture = Fixture::new();
        let mut controller = require_ok(fixture.initialize());
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.hold_commands = true;
            state.writes.clear();
        });
        fixture.write(0x9c, code << 24);
        fixture.write(0x60, 1);
        assert_eq!(controller.synchronize(), Err(Error::Global(1)));
        let failure = require_some(controller.failure());
        assert_eq!(
            failure.command_error(),
            match code {
                1 => CommandError::Illegal,
                2 => CommandError::FetchAbort,
                3 => CommandError::AtcInvalidationTimeout,
                _ => CommandError::Other(127),
            }
        );
        assert_eq!(failure.containment, Containment::AbortAcknowledged);
        assert_ne!(fixture.read(0x44) & (1 << 20), 0);
        assert_eq!(fixture.read(0x24), 0);
        STATE.with(|state| assert_eq!(state.borrow().writes, [(0x20, 0), (0x64, 1)]));
        controller.fail_closed(Error::Corrupt);
        assert_eq!(controller.failure(), Some(failure));
    }
}

#[test]
fn failed_abort_handshake_does_not_disable_translation_or_free_backing() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let domain = require_ok(controller.create_domain());
    let page = require_ok(TestEnvironment::allocate(0));
    require_ok(controller.map_page(domain, 0x2000, page, Permissions::ReadWrite));
    require_ok(controller.attach(8, domain));
    let live = STATE.with(|state| state.borrow().live_pages);
    STATE.with(|state| state.borrow_mut().hold_update = true);
    fixture.write(0x60, 1 << 8);
    assert_eq!(controller.next_event(), Err(Error::Global(1 << 8)));
    assert!(matches!(
        require_some(controller.failure()).containment,
        Containment::Unconfirmed(Error::Timeout { register: 0x44, .. })
    ));
    assert_eq!(fixture.read(0x20) & 1, 1);
    assert_eq!(fixture.read(0x64), 0);
    drop(controller);
    assert_eq!(STATE.with(|state| state.borrow().live_pages), live);
}

#[test]
fn disable_timeout_cannot_be_reported_as_successful_containment() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    STATE.with(|state| state.borrow_mut().hold_disable = true);
    controller.fail_closed(Error::EventOverflow);
    assert!(matches!(
        require_some(controller.failure()).containment,
        Containment::Unconfirmed(Error::Timeout { register: 0x24, .. })
    ));
    assert_eq!(controller.create_domain(), Err(Error::Failed));
}

#[test]
fn global_error_classes_all_close_admission_and_retain_memory() {
    for error in [
        1 << 2,
        1 << 3,
        1 << 4,
        1 << 5,
        1 << 6,
        1 << 7,
        1 << 8,
        1 << 9,
        1 << 10,
    ] {
        let fixture = Fixture::new();
        let mut controller = require_ok(fixture.initialize());
        let live = STATE.with(|state| state.borrow().live_pages);
        fixture.write(0x60, error);
        assert_eq!(controller.next_event(), Err(Error::Global(error)));
        assert_eq!(require_some(controller.failure()).global_errors, error);
        assert_eq!(
            require_some(controller.failure()).containment,
            Containment::AbortAcknowledged
        );
        assert_eq!(controller.create_domain(), Err(Error::Failed));
        drop(controller);
        assert_eq!(STATE.with(|state| state.borrow().live_pages), live);
    }
}

#[test]
fn irq_enable_timeout_is_contained_without_releasing_control_memory() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    STATE.with(|state| state.borrow_mut().hold_irq = true);
    assert!(matches!(
        controller.enable_interrupts(),
        Err(Error::Timeout { register: 0x54, .. })
    ));
    assert_eq!(
        require_some(controller.failure()).containment,
        Containment::AbortAcknowledged
    );
    assert_eq!(controller.enable_interrupts(), Err(Error::Failed));
}

#[test]
fn overflow_and_unattributable_events_close_the_whole_controller() {
    for event in [
        Event {
            words: [0x7f, 0, 0, 0],
        },
        translation_fault(64),
        Event {
            words: [0x10, 1 << 31, 0, 0],
        },
    ] {
        let fixture = Fixture::new();
        let mut controller = require_ok(fixture.initialize());
        assert_eq!(
            controller.quarantine_fault(event),
            Err(Error::UnexpectedEvent)
        );
        assert_eq!(
            require_some(controller.failure()).containment,
            Containment::AbortAcknowledged
        );
    }
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    fixture.write(0x100a8, 1 << 31);
    assert_eq!(controller.next_event(), Err(Error::EventOverflow));
    let failure = require_some(controller.failure());
    assert_eq!(failure.event_producer, 1 << 31);
    assert_eq!(failure.containment, Containment::AbortAcknowledged);
}

#[test]
fn partial_stream_quarantine_fails_closed_and_preserves_ownership() {
    let fixture = Fixture::new();
    let mut controller = require_ok(fixture.initialize());
    let id = require_ok(controller.create_domain());
    require_ok(controller.attach(8, id));
    let live = STATE.with(|state| state.borrow().live_pages);
    STATE.with(|state| state.borrow_mut().hold_commands = true);
    assert!(matches!(
        controller.quarantine_fault(translation_fault(8)),
        Err(Error::Timeout { .. })
    ));
    assert!(controller.stream_quarantined(8));
    assert_eq!(
        require_some(controller.failure()).event,
        Some(translation_fault(8))
    );
    assert_eq!(
        require_some(controller.failure()).containment,
        Containment::AbortAcknowledged
    );
    drop(controller);
    assert_eq!(STATE.with(|state| state.borrow().live_pages), live);
}

#[test]
fn event_decode_distinguishes_permission_and_walk_abort() {
    let mut event = translation_fault(8);
    assert_eq!(event.fault(), FaultKind::Translation);
    event.words[0] = (8 << 32) | 0x0b;
    assert_eq!(event.fault(), FaultKind::TranslationFetch);
    event.words[0] = (8 << 32) | 0x13;
    assert_eq!(event.fault(), FaultKind::Permission);
    event.words[1] |= 1 << 35;
    assert!(event.read());
    assert!(!event.stalled());
    assert_eq!(event.address(), 0x3000);
}

#[test]
fn wired_irq_routes_support_separate_and_combined_but_reject_ambiguous_firmware() {
    use hyper::drivers::iommu::smmuv3::firmware::wired_interrupts;
    let routes = require_ok(wired_interrupts(
        b"gerror\0eventq\0priq\0cmdq-sync\0",
        &[0, 77, 1, 0, 74, 1, 0, 75, 1, 0, 76, 1],
    ));
    assert_eq!(routes.event, [0, 74, 1]);
    assert_eq!(routes.global_error, Some([0, 77, 1]));
    let combined = require_ok(wired_interrupts(b"combined\0", &[0, 74, 4]));
    assert_eq!(combined.event, [0, 74, 4]);
    assert_eq!(combined.global_error, None);
    for (names, cells) in [
        (&b"eventq\0"[..], &[][..]),
        (&b"eventq\0"[..], &[0, 74, 1][..]),
        (&b"eventq\0gerror"[..], &[0, 74, 1, 0, 77, 1][..]),
        (&b"eventq\0gerror\0"[..], &[0, 74, 1, 0, 77][..]),
        (&b"eventq\0gerror\0"[..], &[0, 74, 1, 0, 74, 1][..]),
        (&b"eventq\0gerror\0"[..], &[1, 74, 1, 0, 77, 1][..]),
        (&b"eventq\0gerror\0"[..], &[0, 74, 2, 0, 77, 1][..]),
        (&b"eventq\0eventq\0"[..], &[0, 74, 1, 0, 77, 1][..]),
        (&b"combined\0gerror\0"[..], &[0, 74, 1, 0, 77, 1][..]),
        (&b"combined\0"[..], &[0, 988, 1][..]),
    ] {
        assert_eq!(wired_interrupts(names, cells), Err(Error::Unsupported));
    }
}

#[test]
fn wired_interrupt_selection_clears_firmware_msi_targets() {
    let fixture = Fixture::new();
    fixture.write(0x68, 0x1000);
    fixture.write(0xb0, 0x2000);
    let mut controller = require_ok(fixture.initialize());
    assert_eq!(fixture.read(0x68), 0);
    assert_eq!(fixture.read(0xb0), 0);
    require_ok(controller.enable_interrupts());
    assert_eq!(fixture.read(0x54), 5);
}
