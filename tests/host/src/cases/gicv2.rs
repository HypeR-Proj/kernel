// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::drivers::interrupt::gicv2::{GicV2, SgiCompletions};
use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};
use hyper::hal::interrupt::{
    InterruptId, InterruptPriority, InterruptTrigger, LocalInterruptController,
};
use hyper::platform::{GicV2Info, PhysicalRange};
struct TestBarrier;
impl Barrier for TestBarrier {
    fn data_memory(_: BarrierDomain, _: BarrierAccess) {}
    fn data_synchronization(_: BarrierDomain, _: BarrierAccess) {}
    fn instruction_synchronization() {}
}
static COMPLETIONS: SgiCompletions = SgiCompletions::new();

#[test]
fn banked_targets_and_sgi_sender_survive_acknowledgement() {
    let mut distributor = vec![0u32; 1024];
    let mut cpu = vec![0u32; 1024];
    distributor[1] = 2; // 96 IRQs
    distributor[0x800 / 4] = 0x04040404; // CPU target bit 2, not CPU index zero
    let d = distributor.as_mut_ptr() as usize;
    let c = cpu.as_mut_ptr() as usize;
    let info = GicV2Info {
        distributor: crate::require_some(PhysicalRange::new(0x10000, 4096)),
        cpu_interface: crate::require_some(PhysicalRange::new(0x20000, 4096)),
    };
    // SAFETY: Aligned vectors remain live throughout all emulated MMIO accesses.
    let mut gic = crate::require_ok(unsafe {
        GicV2::<TestBarrier>::bind(
            info,
            |p| match p {
                0x10000 => Some(d),
                0x20000 => Some(c),
                _ => None,
            },
            &COMPLETIONS,
        )
    });
    // SAFETY: This test exclusively owns both simulated register banks.
    crate::require_ok(unsafe { gic.initialize() });
    assert_eq!(gic.interrupt_count(), 96);
    assert_eq!(distributor[0x820 / 4], 0x04040404);
    assert_eq!(cpu[0], 1);
    assert_eq!(distributor[0x80 / 4], 0);
    assert_eq!(distributor[0x84 / 4], 0);
    let local = gic.local_controller();
    assert_eq!(crate::require_ok(local.target()), 4);
    crate::require_ok(local.configure(
        InterruptId::new(26),
        InterruptPriority::High,
        InterruptTrigger::Level,
    ));
    assert!(
        local
            .configure(
                InterruptId::new(8),
                InterruptPriority::High,
                InterruptTrigger::Level
            )
            .is_err()
    );
    assert!(local.enable(InterruptId::new(32)).is_err());
    assert!(gic.enable(InterruptId::new(96)).is_err());
    crate::require_ok(gic.enable(InterruptId::new(40)));
    assert_eq!(distributor[0x104 / 4], 1 << 8);
    assert!(local.send_sgi(InterruptId::new(8), 0x20));
    assert_eq!(distributor[0xf00 / 4], 0x00200008);
    assert!(local.broadcast_sgi(InterruptId::new(9)));
    assert_eq!(distributor[0xf00 / 4], 0x01000009);
    assert!(!local.send_sgi(InterruptId::new(16), 1));
    cpu[0xc / 4] = (5 << 10) | 8;
    let irq = crate::require_some(local.acknowledge());
    assert_eq!(irq.get(), 8);
    // Nested higher-priority SGI from another source must not destroy IRQ8's token.
    cpu[0xc / 4] = (1 << 10) | 9;
    let nested = crate::require_some(local.acknowledge());
    local.end(nested);
    assert_eq!(cpu[0x10 / 4], (1 << 10) | 9);
    local.end(irq);
    assert_eq!(cpu[0x10 / 4], (5 << 10) | 8);
    distributor[0x800 / 4] = 0;
    assert_eq!(crate::require_ok(local.target()), 1); // UP RAZ/WI
    distributor[1] |= 1 << 5; // Multiple CPU interfaces cannot use this fallback.
    assert!(local.target().is_err());
    distributor[0x800 / 4] = 3;
    assert!(local.target().is_err());
    distributor[0x800 / 4] = 0x04040404;
    distributor[1] |= 1 << 10; // Security Extensions, simulated NS bank.
    // SAFETY: Test still owns the emulated registers, with no IRQ execution.
    crate::require_ok(unsafe { gic.initialize() });
    assert_eq!(distributor[0x80 / 4], u32::MAX);
    assert_eq!(distributor[0x84 / 4], u32::MAX);
    for spurious in 1020..=1023 {
        cpu[0xc / 4] = spurious;
        assert!(local.acknowledge().is_none());
    }
}
