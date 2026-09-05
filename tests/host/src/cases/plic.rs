// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RISC-V PLIC register ordering contracts.

use core::ptr::write_volatile;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use hyper::drivers::interrupt::plic::Plic;
use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};
use hyper::hal::interrupt::{
    InterruptController, InterruptId, InterruptPriority, InterruptTrigger,
    KernelInterruptController,
};
use hyper::platform::{MAX_PLIC_CONTEXTS, PhysicalRange, PlicInfo};

const REGISTER_SIZE: usize = 0x20_1000;
const CLAIM_OFFSET: usize = 0x20_0004;
const MAX_EVENTS: usize = 8;

static EVENT_COUNT: AtomicUsize = AtomicUsize::new(0);
static EVENTS: [AtomicU8; MAX_EVENTS] = [const { AtomicU8::new(0) }; MAX_EVENTS];

struct TestBarrier;

impl Barrier for TestBarrier {
    fn data_memory(_domain: BarrierDomain, access: BarrierAccess) {
        let event = match access {
            BarrierAccess::Reads => 1,
            BarrierAccess::Writes => 2,
            BarrierAccess::All => 3,
        };
        let index = EVENT_COUNT.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = EVENTS.get(index) {
            slot.store(event, Ordering::Relaxed);
        }
    }

    fn data_synchronization(_domain: BarrierDomain, _access: BarrierAccess) {}

    fn instruction_synchronization() {}
}

#[test]
fn uses_operation_specific_plic_barriers() {
    const PHYSICAL_BASE: u64 = 0x1000_0000;
    let mut registers = vec![0u64; REGISTER_SIZE.div_ceil(8)];
    let base = registers.as_mut_ptr() as usize;
    let mut contexts = [0u32; MAX_PLIC_CONTEXTS];
    contexts[0] = 0;
    let info = PlicInfo {
        registers: crate::require_some(PhysicalRange::new(PHYSICAL_BASE, REGISTER_SIZE as u64)),
        source_count: 63,
        supervisor_contexts: contexts,
        context_count: 1,
    };
    // SAFETY: The mapping closure returns the complete aligned test bank,
    // which remains exclusively owned and live for the controller's lifetime.
    let mut plic =
        crate::require_ok(unsafe { Plic::<TestBarrier>::bind(info, |_| Some(base), || 0) });

    assert_events(&[2, 2]);
    crate::require_ok(plic.configure(
        InterruptId::new(5),
        InterruptPriority::Normal,
        InterruptTrigger::Level,
    ));
    assert_events(&[2, 2]);

    crate::require_ok(plic.enable(InterruptId::new(5)));
    assert_events(&[3, 1, 2, 2]);

    // SAFETY: CLAIM is an aligned u32 register inside the live test bank.
    unsafe { write_volatile(base.wrapping_add(CLAIM_OFFSET) as *mut u32, 5) };
    assert_eq!(plic.acknowledge(), Some(InterruptId::new(5)));
    assert_events(&[3, 1]);

    plic.end(InterruptId::new(5));
    assert_events(&[3, 2]);
}

fn assert_events(expected: &[u8]) {
    let count = EVENT_COUNT.swap(0, Ordering::Relaxed);
    assert_eq!(count, expected.len());
    for (slot, expected) in EVENTS.iter().zip(expected) {
        assert_eq!(slot.load(Ordering::Relaxed), *expected);
    }
}
