// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RISC-V vCPU hardware-state mechanisms.

use core::arch::asm;
use hyper::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

struct ActiveOwner {
    context: AtomicUsize,
    root: AtomicU64,
}
static OWNERS: [ActiveOwner; hyper::config::MAX_CPUS as usize] = [const {
    ActiveOwner {
        context: AtomicUsize::new(0),
        root: AtomicU64::new(0),
    }
};
    hyper::config::MAX_CPUS as usize];

pub(crate) fn stage2_root_is_active(root: u64) -> bool {
    let owner = &OWNERS[crate::arch::riscv64::current_cpu_index()];
    owner.context.load(Ordering::Acquire) != 0 && owner.root.load(Ordering::Relaxed) == root
}

pub(crate) fn owns(context: &VcpuContext) -> bool {
    OWNERS[crate::arch::riscv64::current_cpu_index()]
        .context
        .load(Ordering::Acquire)
        == core::ptr::from_ref(context).addr()
}

use {crate::arch::riscv64::VcpuContext, crate::arch::riscv64::VmInterruptController};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    SupervisorTimerCompareUnavailable,
    Owner,
    Stage2NotSelected,
    Controller(crate::arch::riscv64::vm::interrupt::Error),
    GuestRun(crate::arch::riscv64::context::GuestRunError),
}

/// Loads one exclusively owned stopped vCPU into the current hart.
///
/// # Safety
///
/// `context` must be pinned and exclusively owned, no vCPU may already be
/// active on this hart, and local interrupts must be masked.
pub unsafe fn activate(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    _physical_count: u64,
) -> Result<bool, Error> {
    let owner = &OWNERS[crate::arch::riscv64::current_cpu_index()];
    if owner.context.load(Ordering::Acquire) != 0 {
        return Err(Error::Owner);
    }
    let hgatp: u64;
    // SAFETY: The caller must have installed this VM's stage-2 selection while
    // keeping IRQs masked. Validate the hardware, not a cached residency epoch:
    // detachment deliberately removes HGATP even for a later same-VM entry.
    unsafe {
        asm!("csrr {value}, hgatp", value = out(reg) hgatp, options(nomem, nostack));
    }
    let root = (hgatp & ((1u64 << 44) - 1)) << 12;
    if hgatp >> 60 != 8 || root == 0 || root & 0x3fff != 0 {
        return Err(Error::Stage2NotSelected);
    }
    if !enable_supervisor_timer_compare() {
        return Err(Error::SupervisorTimerCompareUnavailable);
    }
    reconcile_saved(context, vcpu_id, interrupts)?;
    // SAFETY: The caller grants exclusive ownership of the stopped context.
    unsafe { context.activate_system_registers() };
    owner.root.store(root, Ordering::Relaxed);
    owner
        .context
        .store(core::ptr::from_mut(context).addr(), Ordering::Release);
    Ok(false)
}

pub(crate) fn discover_local_timer() -> bool {
    let accepted: u64;
    // SAFETY: CPU admission has already validated the H extension and keeps
    // IRQs masked. Probe only the timer gate, then restore the complete host
    // environment before publishing this CPU as compatible. No VSTIMECMP
    // access is attempted unless firmware granted MENVCFG.STCE.
    unsafe {
        asm!("csrr {original}, henvcfg", "csrs henvcfg, {stce}", "csrr {accepted}, henvcfg", "csrw henvcfg, {original}", original = out(reg) _, accepted = out(reg) accepted, stce = in(reg) 1u64 << 63, options(nomem, nostack));
    }
    accepted & (1 << 63) != 0
}

fn enable_supervisor_timer_compare() -> bool {
    let environment: u64;
    // SAFETY: Admission verified firmware timer delegation on every online
    // hart. Keep the readback here as a fail-closed ownership boundary.
    unsafe {
        asm!("csrs henvcfg, {stce}", "csrr {environment}, henvcfg", stce = in(reg) 1u64 << 63, environment = out(reg) environment, options(nomem, nostack));
    }
    environment & (1 << 63) != 0
}

/// Saves the current hart's guest state into its exclusively owned context.
///
/// # Safety
///
/// `context` must own the active local vCPU and local interrupts must remain
/// masked until the save completes.
pub unsafe fn deactivate(
    context: &mut VcpuContext,
    _vcpu_id: u32,
    _interrupts: &VmInterruptController,
    _physical_count: u64,
) -> Result<(), Error> {
    if !owns(context) {
        return Err(Error::Owner);
    }
    // SAFETY: The caller identifies this context as the active local vCPU.
    unsafe { context.deactivate_system_registers() };
    quiesce_virtual_interrupt_delivery();
    // SAFETY: The exact local owner is stopped, so its translation bank can be retired.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +h",
            "csrw hgatp, zero",
            "hfence.gvma zero, zero",
            "csrw htimedelta, zero",
            ".option pop",
            options(nostack)
        );
    }
    OWNERS[crate::arch::riscv64::current_cpu_index()]
        .context
        .store(0, Ordering::Release);
    Ok(())
}

pub const fn handle_virtual_timer_interrupt(
    _context: &mut VcpuContext,
    _vcpu_id: u32,
    _interrupts: &VmInterruptController,
) -> Result<bool, Error> {
    Ok(false)
}

pub const fn handle_maintenance_interrupt(
    _context: &mut VcpuContext,
    _vcpu_id: u32,
    _interrupts: &VmInterruptController,
) -> Result<bool, Error> {
    Ok(false)
}

pub const fn maintenance_interrupt_pending() -> bool {
    false
}

pub fn quiesce_virtual_interrupt_delivery() {
    // SAFETY: Called on an admitted HS hart with interrupts masked and no guest executing.
    unsafe {
        asm!(".option push", ".option arch, +h", "csrw vsie, zero", "csrw vsatp, zero", "hfence.vvma zero, zero", "csrc hvip, {mask}", "csrw 0x24d, {disabled}", ".option pop", mask = in(reg) crate::arch::riscv64::registers::HVIP_GUEST_MASK, disabled = in(reg) u64::MAX, options(nostack));
    }
}

/// Detaches exactly the stopped run represented by the linear proof.
///
/// # Safety
/// The context is pinned, owns this hart's active lease, and IRQs remain masked.
pub unsafe fn deactivate_stopped(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
    mut stopped: crate::arch::riscv64::context::StoppedGuestRun,
) -> Result<(), StoppedDeactivationFailure> {
    if let Err(error) = stopped.validate_for(context) {
        return Err(StoppedDeactivationFailure {
            error: Error::GuestRun(error),
            _stopped: stopped,
        });
    }
    // SAFETY: Proof validation identifies the exact fully captured local owner.
    if let Err(error) = unsafe { deactivate(context, vcpu_id, interrupts, physical_count) } {
        return Err(StoppedDeactivationFailure {
            error,
            _stopped: stopped,
        });
    }
    if let Err(error) = stopped.consume_for(context) {
        return Err(StoppedDeactivationFailure {
            error: Error::GuestRun(error),
            _stopped: stopped,
        });
    }
    Ok(())
}

pub struct StoppedDeactivationFailure {
    error: Error,
    _stopped: crate::arch::riscv64::context::StoppedGuestRun,
}
impl StoppedDeactivationFailure {
    pub const fn error(&self) -> Error {
        self.error
    }
}

fn reconcile_saved(
    context: &mut VcpuContext,
    vcpu: u32,
    interrupts: &VmInterruptController,
) -> Result<(), Error> {
    let pending = interrupts
        .external_pending(vcpu)
        .map_err(Error::Controller)?;
    // The PLIC owns only the level-driven external source. A guest-cleared
    // software interrupt and the SSTC compare retain independent ownership.
    context.hvip = (context.hvip & !(1 << 10)) | (u64::from(pending) << 10);
    Ok(())
}

pub(crate) fn update_guest_device_interrupt(
    context: &mut VcpuContext,
    vcpu: u32,
    interrupts: &VmInterruptController,
    interrupt: hyper_vm_models::interrupt::VirtualInterruptId,
    asserted: bool,
) -> Result<(), Error> {
    if !owns(context) {
        return Err(Error::Owner);
    }
    update_saved_guest_device_interrupt(interrupts, vcpu, interrupt, asserted)?;
    reconcile_saved(context, vcpu, interrupts)
}

pub(crate) fn update_saved_guest_device_interrupt(
    interrupts: &VmInterruptController,
    vcpu: u32,
    interrupt: hyper_vm_models::interrupt::VirtualInterruptId,
    asserted: bool,
) -> Result<(), Error> {
    interrupts
        .set_device_level(vcpu, interrupt.get(), asserted)
        .map_err(Error::Controller)
}

pub(crate) fn access_plic(
    context: &mut VcpuContext,
    interrupts: &VmInterruptController,
    vcpu: u32,
    offset: u64,
    size: usize,
    operation: hyper_vm_models::exit::MmioOperation,
) -> Result<Option<u64>, Error> {
    if !owns(context) {
        return Err(Error::Owner);
    }
    let result = interrupts
        .access_plic(vcpu, offset, size, operation)
        .map_err(Error::Controller)?;
    reconcile_saved(context, vcpu, interrupts)?;
    Ok(result)
}

pub(crate) struct RiscvWfiState {
    pub interrupt_may_wake: bool,
    pub timer: hyper_vm_models::riscv64::time::TimerWake,
}

pub(crate) fn stopped_guest_wfi_state(
    context: &VcpuContext,
    vcpu: u32,
    interrupts: &VmInterruptController,
    physical_count: u64,
) -> Result<RiscvWfiState, Error> {
    let external = interrupts
        .external_pending(vcpu)
        .map_err(Error::Controller)?;
    let pending = (context.hvip & !(1 << 10)) | (u64::from(external) << 10);
    let timer = hyper_vm_models::riscv64::time::timer_wake(
        physical_count,
        context.virtual_count_offset(),
        context.vstimecmp,
        context.vsie & (1 << 5) != 0,
    );
    Ok(RiscvWfiState {
        interrupt_may_wake: ((pending >> 1) & context.vsie) != 0
            || matches!(timer, hyper_vm_models::riscv64::time::TimerWake::PendingNow),
        timer,
    })
}

pub(crate) fn request_guest_exit(cpu: hyper::cpu::CpuIndex) -> bool {
    crate::arch::riscv64::notify_reschedule(cpu)
}
