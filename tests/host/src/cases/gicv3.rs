// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! GICv3 CPU-interface initialization through explicit hardware capabilities.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use hyper::drivers::interrupt::gicv3::{CpuInterface, Error as GicError, GicV3};
use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};
use hyper::hal::interrupt::{
    InterruptController, InterruptId, InterruptPriority, InterruptTransitionError, InterruptTrigger,
};
use hyper::hal::timer::MonotonicCounter;
use hyper::platform::{GicV3Info, MAX_GIC_REDISTRIBUTOR_REGIONS, PhysicalRange, RegionList};

static CPU_INITIALIZED: AtomicBool = AtomicBool::new(false);
static ACKNOWLEDGED: AtomicU32 = AtomicU32::new(40);
static COMPLETED: AtomicU32 = AtomicU32::new(u32::MAX);
static CURRENT_AFFINITY: AtomicU32 = AtomicU32::new(0);
static COUNTER_TICKS: AtomicU64 = AtomicU64::new(0);

struct TestCpuInterface;

impl CpuInterface for TestCpuInterface {
    unsafe fn initialize() -> bool {
        CPU_INITIALIZED.store(true, Ordering::Release);
        true
    }

    fn acknowledge() -> u32 {
        ACKNOWLEDGED.load(Ordering::Acquire)
    }

    fn end(interrupt: u32) {
        COMPLETED.store(interrupt, Ordering::Release);
    }

    fn affinity() -> u32 {
        CURRENT_AFFINITY.load(Ordering::Acquire)
    }
}

struct TestBarrier;

impl Barrier for TestBarrier {
    fn data_memory(_domain: BarrierDomain, _access: BarrierAccess) {}
    fn data_synchronization(_domain: BarrierDomain, _access: BarrierAccess) {}
    fn instruction_synchronization() {}
}

struct TestCounter;

impl MonotonicCounter for TestCounter {
    type Error = ();

    fn frequency_hz() -> Result<u64, Self::Error> {
        Ok(1_000_000_000)
    }

    fn read() -> u64 {
        COUNTER_TICKS.fetch_add(100_000_000, Ordering::Relaxed)
    }
}

#[test]
fn initializes_and_configures_the_boot_cpu_interface() {
    const DISTRIBUTOR_PHYSICAL: u64 = 0x1000_0000;
    const REDISTRIBUTOR_PHYSICAL: u64 = 0x2000_0000;
    const AFFINITY: u32 = 0x0102_0304;
    CURRENT_AFFINITY.store(AFFINITY, Ordering::Release);

    CPU_INITIALIZED.store(false, Ordering::Relaxed);
    COMPLETED.store(u32::MAX, Ordering::Relaxed);
    COUNTER_TICKS.store(0, Ordering::Relaxed);
    let mut distributor = vec![0u64; 0x1_0000 / 8];
    let mut redistributor = vec![0u64; 0x2_0000 / 8];
    let distributor_base = distributor.as_mut_ptr() as usize;
    let redistributor_base = redistributor.as_mut_ptr() as usize;
    // SAFETY: Both aligned vectors exclusively own the addressed storage, and
    // the offsets and access widths remain within their allocated lengths.
    unsafe {
        write_volatile(distributor_base.wrapping_add(0x4) as *mut u32, 1);
        write_volatile(
            redistributor_base.wrapping_add(0x8) as *mut u64,
            (u64::from(AFFINITY) << 32) | (1 << 4),
        );
    }

    let mut redistributors = RegionList::<MAX_GIC_REDISTRIBUTOR_REGIONS>::new();
    crate::require_ok(
        redistributors.insert(crate::require_some(PhysicalRange::new(
            REDISTRIBUTOR_PHYSICAL,
            0x2_0000,
        ))),
    );
    let info = GicV3Info {
        distributor: crate::require_some(PhysicalRange::new(DISTRIBUTOR_PHYSICAL, 0x1_0000)),
        redistributors,
        redistributor_stride: None,
        maintenance_interrupt: None,
    };
    // SAFETY: The mapping closure returns the complete, exclusively owned test
    // banks described by `info`; both vectors outlive the controller.
    let mut controller = crate::require_ok(unsafe {
        GicV3::<TestCpuInterface, TestBarrier, TestCounter>::bind(info, |address| match address {
            DISTRIBUTOR_PHYSICAL => Some(distributor_base),
            REDISTRIBUTOR_PHYSICAL => Some(redistributor_base),
            _ => None,
        })
    });
    // SAFETY: This single-threaded test models masked local interrupts, and no
    // other agent can observe either register bank during initialization.
    crate::require_ok(unsafe { controller.initialize(AFFINITY) });

    assert!(CPU_INITIALIZED.load(Ordering::Acquire));
    assert_eq!(controller.interrupt_count(), 64);
    assert_eq!(
        // SAFETY: The aligned distributor bank remains alive and offset zero
        // contains a complete 32-bit register.
        unsafe { read_volatile(distributor_base as *const u32) },
        0x13
    );

    let interrupt = InterruptId::new(40);
    crate::require_ok(controller.configure(
        interrupt,
        InterruptPriority::High,
        InterruptTrigger::Edge,
    ));
    assert_eq!(
        // SAFETY: Offset 0x428 is within the live distributor test bank and a
        // byte access has no additional alignment requirement.
        unsafe { read_volatile(distributor_base.wrapping_add(0x428) as *const u8) },
        0x40
    );
    assert_ne!(
        // SAFETY: Offset 0xc08 is aligned for u32 and contained in the live
        // distributor test bank.
        unsafe { read_volatile(distributor_base.wrapping_add(0x0c08) as *const u32) } & (1 << 17),
        0
    );
    assert_eq!(
        // SAFETY: Offset 0x6140 is aligned for u64 and contained in the live
        // distributor test bank.
        unsafe { read_volatile(distributor_base.wrapping_add(0x6140) as *const u64) },
        u64::from(AFFINITY)
    );

    crate::require_ok(controller.enable(interrupt));
    assert_eq!(
        // SAFETY: Offset 0x104 is aligned for u32 and contained in the live
        // distributor test bank.
        unsafe { read_volatile(distributor_base.wrapping_add(0x104) as *const u32) },
        1 << 8
    );
    crate::require_ok(controller.disable(interrupt));
    assert_eq!(
        // SAFETY: Offset 0x184 is aligned for u32 and contained in the live
        // distributor test bank.
        unsafe { read_volatile(distributor_base.wrapping_add(0x184) as *const u32) },
        1 << 8
    );

    assert_eq!(
        controller.enable(InterruptId::new(64)),
        Err(InterruptTransitionError::NotApplied(
            GicError::InvalidInterrupt
        ))
    );
    // A stuck RWP bit is observed only after the enable command is written.
    // The typed result must therefore prohibit registry rollback as though no
    // hardware transition had occurred.
    // SAFETY: GICD_CTLR and GICD_ISENABLER1 are aligned and contained in the
    // exclusively owned distributor test bank.
    unsafe {
        write_volatile(distributor_base as *mut u32, 1 << 31);
        write_volatile(distributor_base.wrapping_add(0x104) as *mut u32, 0);
    }
    assert_eq!(
        controller.enable(interrupt),
        Err(InterruptTransitionError::AppliedOrUnknown(
            GicError::RegisterTimeout
        ))
    );
    assert_eq!(
        // SAFETY: The live aligned register bank is unchanged during this read.
        unsafe { read_volatile(distributor_base.wrapping_add(0x104) as *const u32) },
        1 << 8
    );
    // A disable command has the same commit point: failure to observe RWP
    // completion cannot prove that delivery remained enabled.
    // SAFETY: GICD_CTLR and GICD_ICENABLER1 are aligned and contained in the
    // exclusively owned distributor test bank.
    unsafe {
        write_volatile(distributor_base.wrapping_add(0x184) as *mut u32, 0);
    }
    assert_eq!(
        controller.disable(interrupt),
        Err(InterruptTransitionError::AppliedOrUnknown(
            GicError::RegisterTimeout
        ))
    );
    assert_eq!(
        // SAFETY: The live aligned register bank is unchanged during this read.
        unsafe { read_volatile(distributor_base.wrapping_add(0x184) as *const u32) },
        1 << 8
    );
    // SAFETY: Restore the synthetic completion state before controller drop.
    unsafe { write_volatile(distributor_base as *mut u32, 0) };
    assert_eq!(controller.acknowledge(), Some(interrupt));
    controller.end(interrupt);
    assert_eq!(COMPLETED.load(Ordering::Acquire), 40);
}
