// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected physical GIC interface and architecture-local IPI routing.

use core::arch::asm;

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::InterruptId;
use hyper_drivers::interrupt::gicv3::CpuInterface;

use crate::arch::aarch64::registers;

use crate::arch::aarch64::barrier::Aarch64Barrier;
use crate::arch::aarch64::irq::controller::Error;
use core::sync::atomic::{AtomicU64, Ordering};
use hyper::sync::PublishedOnce;
use hyper_drivers::interrupt::gicv2::GicV2Local;

enum Interface {
    V2(GicV2Local<Aarch64Barrier>),
    V3,
}
static INTERFACE: PublishedOnce<Interface> = PublishedOnce::new();
// Indexed by hardware CPU target bit, not by MPIDR Aff0 or logical CPU number.
static V2_AFFINITIES: [AtomicU64; 8] = [const { AtomicU64::new(u64::MAX) }; 8];

pub(crate) fn interface_installed() -> bool {
    INTERFACE.get().is_some()
}
pub(crate) fn install_v3() -> Result<(), Error> {
    INTERFACE
        .publish(Interface::V3)
        .map_err(|_| Error::AlreadyInitialized)
}
pub(crate) fn install_v2(local: GicV2Local<Aarch64Barrier>) -> Result<(), Error> {
    register_v2_cpu(local)?;
    INTERFACE
        .publish(Interface::V2(local))
        .map_err(|_| Error::AlreadyInitialized)
}
pub(crate) fn register_v2_cpu(local: GicV2Local<Aarch64Barrier>) -> Result<(), Error> {
    let mask = local.target()?;
    let slot = &V2_AFFINITIES[mask.trailing_zeros() as usize];
    slot.compare_exchange(
        u64::MAX,
        u64::from(current_gic_affinity()),
        Ordering::Release,
        Ordering::Relaxed,
    )
    .map(|_| ())
    .map_err(|_| Error::AlreadyInitialized)
}
pub fn guest_interrupts_supported() -> bool {
    matches!(INTERFACE.get(), Some(Interface::V3))
        || crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some()
}

pub(crate) fn v2_target(affinity: u32) -> Option<u8> {
    V2_AFFINITIES
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == u64::from(affinity))
        .map(|index| 1 << index)
}

fn send_targeted(interrupt: InterruptId, affinity: u32) -> bool {
    match INTERFACE.get() {
        Some(Interface::V2(local)) => {
            let Some(index) = V2_AFFINITIES
                .iter()
                .position(|slot| slot.load(Ordering::Acquire) == u64::from(affinity))
            else {
                return false;
            };
            local.send_sgi(interrupt, 1 << index)
        }
        Some(Interface::V3) => {
            let Some(value) = targeted_sgi_value(interrupt, affinity) else {
                return false;
            };
            // SAFETY: Published GICv3 setup enabled SRE before admitting targets.
            // Complete durable work publication before sending the doorbell.
            unsafe {
                asm!("dsb ishst", "msr ICC_SGI1R_EL1, {value}", value = in(reg) value, options(nostack, preserves_flags));
            }
            true
        }
        None => false,
    }
}

/// `AArch64` implementation of the `GICv3` system-register CPU interface.
pub struct Aarch64GicCpuInterface;

impl CpuInterface for Aarch64GicCpuInterface {
    unsafe fn initialize() -> bool {
        let mut sre: u64;
        // SAFETY: The boot CPU executes at EL2 with IRQs masked. These system
        // registers configure only its physical GIC CPU interface.
        unsafe {
            asm!(
                "mrs {sre}, ICC_SRE_EL2",
                "orr {sre}, {sre}, #{sre_bit}",
                "msr ICC_SRE_EL2, {sre}",
                "isb",
                "mrs {sre}, ICC_SRE_EL2",
                sre = inout(reg) 0u64 => sre,
                sre_bit = const registers::ICC_SRE_EL2_SRE,
                options(nostack, preserves_flags)
            );
        }
        if sre & registers::ICC_SRE_EL2_SRE == 0 {
            return false;
        }

        let mut control: u64;
        // SAFETY: SRE was confirmed active and the caller exclusively owns the
        // masked local CPU interface during initialization.
        unsafe {
            asm!(
                "msr ICH_HCR_EL2, xzr",
                "mov {control:w}, #{priority}",
                "msr ICC_PMR_EL1, {control}",
                "msr ICC_BPR1_EL1, xzr",
                "mrs {control}, ICC_CTLR_EL1",
                control = out(reg) control,
                priority = const registers::ICC_PMR_ALLOW_ALL,
                options(nostack, preserves_flags)
            );
        }
        control &= !registers::ICC_CTLR_EL1_EOI_MODE;
        // SAFETY: Initialization still owns the masked local interface; this
        // publishes its final priority, EOI, and Group-1 enable state.
        unsafe {
            asm!(
                "msr ICC_CTLR_EL1, {control}",
                "mov {control:w}, #{enable}",
                "msr ICC_IGRPEN1_EL1, {control}",
                "isb",
                control = inout(reg) control => _,
                enable = const registers::ICC_IGRPEN1_ENABLE,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    fn acknowledge() -> u32 {
        let interrupt: u64;
        // SAFETY: Reading IAR1 acknowledges the highest-priority pending Group
        // 1 interrupt for this CPU and does not dereference memory.
        unsafe {
            asm!(
                "mrs {interrupt}, ICC_IAR1_EL1",
                interrupt = out(reg) interrupt,
                options(nostack, preserves_flags)
            );
        }
        (interrupt & registers::ICC_IAR1_INTID_MASK) as u32
    }

    fn end(interrupt: u32) {
        // SAFETY: Writing EOIR is valid at EL2. The controller contract requires
        // the caller to provide an interrupt returned by acknowledge; EOImode is
        // configured so this write also performs deactivation.
        unsafe {
            asm!(
                "msr ICC_EOIR1_EL1, {interrupt}",
                "isb",
                interrupt = in(reg) u64::from(interrupt),
                options(nostack, preserves_flags)
            );
        }
    }

    fn affinity() -> u32 {
        current_gic_affinity()
    }
}

/// Returns the GIC affinity encoding for the current processing element.
pub fn current_gic_affinity() -> u32 {
    let mpidr: u64;
    // SAFETY: MPIDR_EL1 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "mrs {mpidr}, MPIDR_EL1",
            mpidr = out(reg) mpidr,
            options(nomem, nostack, preserves_flags)
        );
    }
    let affinity_0_to_2 = mpidr & registers::MPIDR_AFF0_TO_2_MASK;
    let affinity_3 = (mpidr >> registers::MPIDR_AFF3_SHIFT) & registers::MPIDR_AFF3_MASK;
    (affinity_0_to_2 | (affinity_3 << registers::GIC_AFF3_SHIFT)) as u32
}

/// Returns the architecture-reserved emergency stop interrupt.
pub const fn crash_stop_interrupt() -> Option<InterruptId> {
    Some(InterruptId::new(registers::GIC_CRASH_STOP_SGI as u32))
}

/// Returns the architecture-reserved scheduler reschedule interrupt.
pub const fn reschedule_interrupt() -> Option<InterruptId> {
    Some(InterruptId::new(registers::GIC_RESCHEDULE_SGI as u32))
}

pub const fn kernel_rpc_interrupt() -> Option<InterruptId> {
    Some(InterruptId::new(registers::GIC_KERNEL_RPC_SGI as u32))
}

pub fn notify_kernel_rpc(cpu: CpuIndex, reasons: u8) -> bool {
    if !crate::arch::aarch64::smp::publish_kernel_rpc(cpu, reasons) {
        return true;
    }
    let Some(affinity) = crate::arch::aarch64::smp::gic_affinity(cpu.get()) else {
        return false;
    };
    let Some(interrupt) = kernel_rpc_interrupt() else {
        return false;
    };
    send_targeted(interrupt, affinity)
}

/// Prompts one logical CPU to evaluate its pending reschedule state.
///
/// The durable condition is the scheduler's coalesced pending flag; this SGI
/// only causes prompt IRQ-tail evaluation. `false` reports an unavailable CPU
/// affinity or an Aff0 value that requires unsupported range selection, so the
/// caller can retain the pending flag and use a broader wakeup mechanism.
pub fn notify_reschedule(cpu: CpuIndex) -> bool {
    notify_with_reschedule_sgi(cpu)
}

/// Prompts a running guest to leave lower EL for durable VM reconciliation.
///
/// This shares the already-configured reschedule SGI, whose handler carries
/// no scheduler state. Kernel IRQ-tail policy independently observes the
/// VM-owned reconcile publication.
pub(crate) fn notify_guest_exit(cpu: CpuIndex) -> bool {
    notify_with_reschedule_sgi(cpu)
}

fn notify_with_reschedule_sgi(cpu: CpuIndex) -> bool {
    let Some(affinity) = crate::arch::aarch64::smp::gic_affinity(cpu.get()) else {
        return false;
    };
    send_targeted(
        InterruptId::new(registers::GIC_RESCHEDULE_SGI as u32),
        affinity,
    )
}

fn targeted_sgi_value(interrupt: InterruptId, affinity: u32) -> Option<u64> {
    if interrupt.get() >= 16 {
        return None;
    }
    let affinity_0 = affinity & 0xff;
    // Range selection requires both Distributor and CPU-interface RSS
    // support. The current profile deliberately rejects that topology rather
    // than writing an implementation-defined RS value or misrouting the SGI.
    if affinity_0 >= 16 {
        return None;
    }
    let affinity_1 = (affinity >> 8) & 0xff;
    let affinity_2 = (affinity >> 16) & 0xff;
    let affinity_3 = (affinity >> 24) & 0xff;
    Some(
        (u64::from(affinity_3) << registers::ICC_SGI1R_AFF3_SHIFT)
            | (u64::from(affinity_2) << registers::ICC_SGI1R_AFF2_SHIFT)
            | (u64::from(interrupt.get()) << registers::ICC_SGI1R_INTID_SHIFT)
            | (u64::from(affinity_1) << registers::ICC_SGI1R_AFF1_SHIFT)
            | (1u64 << affinity_0),
    )
}

/// Tests whether an acknowledged interrupt is the emergency stop IPI.
pub fn is_crash_stop_interrupt(interrupt: InterruptId) -> bool {
    crash_stop_interrupt() == Some(interrupt)
}

/// Sends the emergency stop SGI to every participating PE except self.
pub fn broadcast_crash_stop() -> bool {
    let Some(interrupt) = crash_stop_interrupt() else {
        return false;
    };
    broadcast_sgi(interrupt)
}

fn broadcast_sgi(interrupt: InterruptId) -> bool {
    match INTERFACE.get() {
        Some(Interface::V2(local)) => return local.broadcast_sgi(interrupt),
        Some(Interface::V3) => {}
        None => return false,
    }
    if interrupt.get() >= 16 {
        return false;
    }
    let value =
        (u64::from(interrupt.get()) << registers::ICC_SGI1R_INTID_SHIFT) | registers::ICC_SGI1R_IRM;
    // SAFETY: The GICv3 system-register interface is active before this API is
    // used. IRM routes the SGI to all participating PEs except the caller.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ICC_SGI1R_EL1, {value}",
            "isb",
            value = in(reg) value,
            options(nostack, preserves_flags)
        );
    }
    true
}

/// Acknowledges the highest-priority physical interrupt on this CPU.
pub fn acknowledge_interrupt() -> Option<InterruptId> {
    match INTERFACE.get() {
        Some(Interface::V2(local)) => return local.acknowledge(),
        Some(Interface::V3) => {}
        None => return None,
    }
    let raw = <Aarch64GicCpuInterface as CpuInterface>::acknowledge();
    (raw < registers::GIC_SPURIOUS_INTERRUPT_MIN as u32).then_some(InterruptId::new(raw))
}

/// Completes one interrupt returned by [`acknowledge_interrupt`].
pub fn end_interrupt(interrupt: InterruptId) {
    match INTERFACE.get() {
        Some(Interface::V2(local)) => {
            local.end(interrupt);
            return;
        }
        Some(Interface::V3) => {}
        None => return,
    }
    <Aarch64GicCpuInterface as CpuInterface>::end(interrupt.get());
}
