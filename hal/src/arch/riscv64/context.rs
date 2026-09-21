// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::mem::{offset_of, size_of};

use super::registers;

pub type KernelThreadEntry = extern "C" fn(usize);

#[repr(C, align(16))]
pub struct ThreadContext {
    callee_saved: [u64; 12],
    return_address: u64,
    stack_pointer: u64,
    interrupt_enable: u64,
}

impl ThreadContext {
    pub const fn empty() -> Self {
        Self {
            callee_saved: [0; 12],
            return_address: 0,
            stack_pointer: 0,
            interrupt_enable: registers::SSTATUS_SIE,
        }
    }

    pub fn prepare(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.callee_saved[0] = entry as usize as u64;
        self.callee_saved[1] = argument as u64;
        self.return_address = riscv64_thread_trampoline as *const () as usize as u64;
        self.stack_pointer = (stack_top & !15) as u64;
    }

    pub fn prepare_vcpu(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.prepare(stack_top, entry, argument);
        self.interrupt_enable = 0;
    }
}

#[repr(C, align(16))]
pub struct VcpuContext {
    pub(crate) general: [u64; 32],
    pub(crate) program_counter: u64,
    pub(crate) vsstatus: u64,
    pub(crate) vsie: u64,
    pub(crate) vstvec: u64,
    pub(crate) vsscratch: u64,
    pub(crate) vsepc: u64,
    pub(crate) vscause: u64,
    pub(crate) vstval: u64,
    pub(crate) hvip: u64,
    pub(crate) vsatp: u64,
    pub(crate) vstimecmp: u64,
    pub(crate) scounteren: u64,
    pub(crate) senvcfg: u64,
    virtual_count_offset: u64,
    pub(crate) floating: [u64; 32],
    pub(crate) fcsr: u32,
    _floating_padding: u32,
    run_state: u64,
    pub(crate) supervisor: u64,
    stopped_exit: Option<GuestRunExit>,
}

const GUEST_RUN_READY: u64 = 0;
const GUEST_RUN_RUNNING: u64 = 1;
const GUEST_RUN_IRQ_TAIL: u64 = 2;
const GUEST_RUN_STOPPED: u64 = 3;

#[repr(C)]
struct GuestAnchorExit {
    kind: u64,
    target: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestSynchronousTerminal {
    Undecodable,
    Unsupported(super::guest::UnsupportedGuestExit),
}

/// Typed guest-policy cause for one terminal run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestTerminalCause {
    MemoryFault,
    Mmio,
    Synchronous(GuestSynchronousTerminal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestWaitReason {
    Interrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestAdministrativeStopReason {
    Requested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestTerminalExit {
    cause: GuestTerminalCause,
    syndrome: u64,
    fault_address: u64,
    program_counter: u64,
    processor_state: u64,
    vector: u64,
}

impl GuestTerminalExit {
    pub(super) fn from_frame(
        frame: &super::exception::TrapFrame,
        cause: GuestTerminalCause,
    ) -> Self {
        Self {
            cause,
            syndrome: frame.scause,
            fault_address: frame.stval,
            program_counter: frame.sepc,
            processor_state: frame.sstatus,
            vector: frame.scause,
        }
    }

    pub(crate) const fn cause(self) -> GuestTerminalCause {
        self.cause
    }

    pub(crate) const fn syndrome(self) -> u64 {
        self.syndrome
    }

    pub(crate) const fn fault_address(self) -> u64 {
        self.fault_address
    }

    pub(crate) const fn program_counter(self) -> u64 {
        self.program_counter
    }

    pub(crate) const fn processor_state(self) -> u64 {
        self.processor_state
    }

    pub(crate) const fn vector(self) -> u64 {
        self.vector
    }
}

/// Copied stopped-exit facts which remain valid after local hardware detaches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestRunExit {
    Wait(GuestWaitReason),
    Terminal(GuestTerminalExit),
    AdministrativeStop(GuestAdministrativeStopReason),
}

/// Linear proof that vector entry captured and closed one guest return world.
#[must_use = "a stopped guest run must be detached exactly once"]
pub struct StoppedGuestRun {
    context: *mut VcpuContext,
    exit: GuestRunExit,
    armed: bool,
    not_send_or_sync: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

impl StoppedGuestRun {
    pub(crate) const fn exit(&self) -> GuestRunExit {
        self.exit
    }

    pub(super) fn validate_for(&self, context: &VcpuContext) -> Result<(), GuestRunError> {
        if !self.armed
            || !core::ptr::eq(self.context, context)
            || context.run_state != GUEST_RUN_STOPPED
        {
            return Err(GuestRunError::Owner);
        }
        Ok(())
    }

    pub(super) fn consume_for(&mut self, context: &mut VcpuContext) -> Result<(), GuestRunError> {
        if !core::ptr::eq(self.context, context) || context.run_state != GUEST_RUN_STOPPED {
            return Err(GuestRunError::Owner);
        }
        context.run_state = GUEST_RUN_READY;
        self.armed = false;
        Ok(())
    }
}

impl Drop for StoppedGuestRun {
    fn drop(&mut self) {
        if self.armed {
            let context = crate::arch::exception::capture_crash_context();
            crate::arch::exception::fatal(
                context,
                format_args!("armed RISC-V stopped-guest proof was dropped without detachment"),
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestRunError {
    Owner,
    Return,
    State,
}

impl VcpuContext {
    pub const fn new(program_counter: u64) -> Self {
        // Linux uses the standard LP64D ABI on the supported RV64GC profile.
        // FS=Initial permits the guest to use F/D state; hardware promotes it
        // to Dirty when a floating-point register is modified.
        const VSSTATUS_FS_INITIAL: u64 = 1 << 13;
        Self {
            general: [0; 32],
            program_counter,
            vsstatus: VSSTATUS_FS_INITIAL | (2 << 32),
            vsie: 0,
            vstvec: 0,
            vsscratch: 0,
            vsepc: 0,
            vscause: 0,
            vstval: 0,
            hvip: 0,
            vsatp: 0,
            vstimecmp: u64::MAX,
            scounteren: 0,
            senvcfg: 0,
            virtual_count_offset: 0,
            floating: [0; 32],
            fcsr: 0,
            _floating_padding: 0,
            run_state: GUEST_RUN_READY,
            supervisor: 1,
            stopped_exit: None,
        }
    }

    pub(crate) const fn initialize_virtual_interrupts(
        &mut self,
    ) -> Result<(), VirtualInterruptError> {
        Ok(())
    }
    pub(crate) fn set_virtual_count(&mut self, physical: u64, value: u64) {
        // HTIMEDELTA is added to TIME while V=1, unlike AArch64 CNTVOFF which
        // is subtracted from the physical counter.
        self.virtual_count_offset = value.wrapping_sub(physical);
    }
    /// Loads this stopped vCPU's hart-local floating-point and timer state.
    ///
    /// # Safety
    ///
    /// The caller must exclusively own this pinned context, keep local
    /// interrupts masked, and ensure no other vCPU is active on this hart.
    pub(crate) unsafe fn activate_system_registers(&self) {
        // SAFETY: HTIMEDELTA is writable in HS mode and the input is a plain value.
        unsafe {
            core::arch::asm!(
                "csrw htimedelta, {offset}",
                offset = in(reg) self.virtual_count_offset,
                options(nostack)
            )
        };
    }
    /// Saves hart-local guest state into this vCPU context.
    ///
    /// # Safety
    ///
    /// This context must be the active vCPU on the current hart, exclusively
    /// owned by the caller, with local interrupts masked.
    pub(crate) unsafe fn deactivate_system_registers(&mut self) {
        // Trap entry already captured the complete guest bank before Rust.
    }
    /// Enters the guest represented by `context`.
    ///
    /// # Safety
    ///
    /// `context` must be non-null, aligned, pinned, and exclusively owned by the
    /// active vCPU for the guest-run lifetime. Trap handling may mutate it.
    pub(crate) unsafe fn run(context: *mut Self) -> Result<StoppedGuestRun, GuestRunError> {
        if context.is_null() || !context.is_aligned() {
            return Err(GuestRunError::Owner);
        }
        // Keep only the pinned raw pointer across guest entry and scheduling.
        // The IRQ-tail transition temporarily lends this object to VM
        // deactivation/reactivation, so retaining a Rust reference here would
        // violate exclusive-borrow provenance even though execution is nested.
        loop {
            let status: u64;
            let scratch: u64;
            // SAFETY: These local supervisor CSRs are read-only snapshots.
            unsafe {
                core::arch::asm!("csrr {status}, sstatus", "csrr {scratch}, sscratch", status = out(reg) status, scratch = out(reg) scratch, options(nomem, nostack));
            }
            if status & registers::SSTATUS_SIE != 0 || scratch != 0 {
                return Err(GuestRunError::State);
            }
            // SAFETY: The validated raw pointer remains pinned; this short
            // borrow ends before any trap or scheduler callback may run.
            if !unsafe { super::vm_vcpu::owns(&*context) } {
                return Err(GuestRunError::Owner);
            }
            // SAFETY: The run owner has exclusive access before guest entry;
            // this short reference ends before assembly or policy can borrow it.
            if unsafe { (&mut *context).begin_run() }.is_err() {
                return Err(GuestRunError::State);
            }
            // SAFETY: RUNNING publishes the exact context to the assembly
            // anchor. A typed anchor exit destroys that publication before it
            // returns here with local interrupts still masked.
            let exit = unsafe { riscv64_enter_guest(context.cast_const().cast()) };
            if exit.kind == registers::GUEST_ANCHOR_EXIT_STOPPED {
                // SAFETY: The destroyed anchor returned this exact pinned owner.
                let state = unsafe { &mut *context };
                if state.run_state != GUEST_RUN_STOPPED {
                    return Err(GuestRunError::State);
                }
                let reason = state.stopped_exit.take().ok_or(GuestRunError::Return)?;
                return Ok(StoppedGuestRun {
                    context,
                    exit: reason,
                    armed: true,
                    not_send_or_sync: core::marker::PhantomData,
                });
            }
            // SAFETY: Typed assembly return restored exclusive access; this
            // short reference ends before invoking the scheduler callback.
            let target = match unsafe { (&mut *context).consume_irq_tail(exit) } {
                Ok(target) => target,
                Err(_) => return Err(GuestRunError::Return),
            };
            // SAFETY: Trap dispatch accepts only an opaque callback previously
            // qualified by the selected HAL. The anchor is gone, no raw frame
            // borrow remains, and SIE stays masked across the callback.
            let postlude: unsafe extern "C" fn() = unsafe { core::mem::transmute(target) };
            // SAFETY: The qualified callback contract is established above.
            unsafe { postlude() };
        }
    }

    /// Captures guest state before a typed IRQ-tail anchor unwind.
    ///
    /// # Safety
    ///
    /// `self` must be the exact context published in the current hart's live
    /// guest anchor. Guest floating-point state must already have been copied
    /// by trap entry and local interrupts must remain masked.
    pub(crate) unsafe fn capture_irq_tail(
        &mut self,
        general: &[u64; 32],
        program_counter: u64,
    ) -> Result<(), GuestAnchorError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestAnchorError::State);
        }
        self.general = *general;
        self.program_counter = program_counter;
        // This state is the final publication consumed by `enter`; all guest
        // register copies happen-before it in same-hart program order.
        self.publish_irq_tail()
    }

    fn begin_run(&mut self) -> Result<(), GuestAnchorError> {
        if self.run_state != GUEST_RUN_READY {
            return Err(GuestAnchorError::State);
        }
        self.run_state = GUEST_RUN_RUNNING;
        Ok(())
    }

    fn publish_irq_tail(&mut self) -> Result<(), GuestAnchorError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestAnchorError::State);
        }
        self.run_state = GUEST_RUN_IRQ_TAIL;
        Ok(())
    }

    fn consume_irq_tail(&mut self, exit: GuestAnchorExit) -> Result<usize, GuestAnchorError> {
        if self.run_state != GUEST_RUN_IRQ_TAIL
            || exit.kind != registers::GUEST_ANCHOR_EXIT_IRQ_TAIL
            || exit.target == 0
        {
            return Err(GuestAnchorError::Exit);
        }
        self.run_state = GUEST_RUN_READY;
        Ok(exit.target)
    }

    pub(super) fn stop(
        &mut self,
        general: &[u64; 32],
        pc: u64,
        exit: GuestRunExit,
    ) -> Result<(), GuestRunError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestRunError::State);
        }
        self.general = *general;
        self.program_counter = pc;
        self.stopped_exit = Some(exit);
        self.run_state = GUEST_RUN_STOPPED;
        Ok(())
    }

    pub(super) const fn virtual_count_offset(&self) -> u64 {
        self.virtual_count_offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestAnchorError {
    State,
    Exit,
}

pub(super) fn validate_anchor_state_machine() -> bool {
    let mut context = VcpuContext::new(0);
    let mut other = VcpuContext::new(0);
    context.set_virtual_count(0xf000, 0x1000);
    if 0xf000u64.wrapping_add(context.virtual_count_offset) != 0x1000 {
        return false;
    }
    if context.begin_run().is_err()
        || context.publish_irq_tail().is_err()
        || other
            .consume_irq_tail(GuestAnchorExit {
                kind: registers::GUEST_ANCHOR_EXIT_IRQ_TAIL,
                target: 1,
            })
            .is_ok()
        || context.consume_irq_tail(GuestAnchorExit {
            kind: registers::GUEST_ANCHOR_EXIT_IRQ_TAIL,
            target: 1,
        }) != Ok(1)
    {
        return false;
    }
    if context
        .consume_irq_tail(GuestAnchorExit {
            kind: registers::GUEST_ANCHOR_EXIT_IRQ_TAIL,
            target: 1,
        })
        .is_ok()
    {
        return false;
    }
    let exit = GuestRunExit::Wait(GuestWaitReason::Interrupt);
    if context.stop(&[0; 32], 4, exit).is_ok()
        || context.begin_run().is_err()
        || context.stop(&[0; 32], 4, exit).is_err()
    {
        return false;
    }
    let mut stopped = StoppedGuestRun {
        context: &mut context,
        exit,
        armed: true,
        not_send_or_sync: core::marker::PhantomData,
    };
    let valid = stopped.validate_for(&other).is_err()
        && stopped.validate_for(&context).is_ok()
        && context.begin_run().is_err()
        && stopped.consume_for(&mut other).is_err()
        && stopped.consume_for(&mut context).is_ok()
        && stopped.validate_for(&context).is_err()
        && stopped.consume_for(&mut context).is_err();
    // This is a locally constructed pure validation witness, not an actual
    // hardware lease. Avoid turning a failed assertion into a destructor trap.
    stopped.armed = false;
    valid && context.begin_run().is_ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtualInterruptError {}

unsafe extern "C" {
    fn riscv64_switch_context(
        previous: *mut ThreadContext,
        next: *const ThreadContext,
        previous_interrupt_state: usize,
        completion: extern "C" fn(usize),
        completion_ticket: usize,
    );
    fn riscv64_thread_trampoline();
    fn riscv64_reset_stack_and_enter(
        bottom: usize,
        top: usize,
        watermark: u64,
        canary: u64,
        callback: extern "C" fn(usize) -> !,
        argument: usize,
    ) -> !;
    fn riscv64_run_on_stack(top: usize, callback: extern "C" fn(usize) -> !, argument: usize) -> !;
    fn riscv64_enter_guest(context: *const u8) -> GuestAnchorExit;
}

/// Switches from `previous` to `next` without returning through a normal call.
///
/// # Safety
///
/// Both pointers must be valid pinned scheduler contexts, `previous` must be
/// uniquely writable, and `next` must contain a saved or prepared context. No
/// Rust reference may remain live because `completion` re-enters scheduler
/// ownership. Local interrupts must be masked and the callback must not switch.
pub unsafe fn switch_thread_context(
    previous: *mut ThreadContext,
    next: *const ThreadContext,
    previous_interrupt_state: usize,
    completion: extern "C" fn(usize),
    completion_ticket: usize,
) {
    // SAFETY: The caller establishes ownership and lifetime for both contexts.
    unsafe {
        riscv64_switch_context(
            previous,
            next,
            previous_interrupt_state,
            completion,
            completion_ticket,
        )
    };
}

/// Resets a stack and transfers control to `callback`.
///
/// # Safety
///
/// `[bottom, top)` must be exclusively owned, writable stack memory. `top`
/// must meet the RISC-V ABI alignment requirement and callback must not return.
pub unsafe fn reset_stack_and_enter(
    bottom: usize,
    top: usize,
    watermark: u64,
    canary: u64,
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The caller provides an exclusive valid stack and non-returning callback.
    unsafe { riscv64_reset_stack_and_enter(bottom, top, watermark, canary, callback, argument) }
}

/// Transfers control to a non-returning callback on another stack.
///
/// # Safety
///
/// `top` must be the aligned top of a live, exclusively owned writable stack.
pub unsafe fn run_on_stack(top: usize, callback: extern "C" fn(usize) -> !, argument: usize) -> ! {
    // SAFETY: The caller provides an aligned, live, exclusively owned stack.
    unsafe { riscv64_run_on_stack(top, callback, argument) }
}

const _: () = {
    assert!(
        offset_of!(ThreadContext, callee_saved) == registers::THREAD_CONTEXT_S0_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, return_address) == registers::THREAD_CONTEXT_RA_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, stack_pointer) == registers::THREAD_CONTEXT_SP_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, interrupt_enable)
            == registers::THREAD_CONTEXT_SIE_OFFSET as usize
    );
    assert!(size_of::<ThreadContext>() == registers::THREAD_CONTEXT_SIZE as usize);
    assert!(offset_of!(VcpuContext, general) == registers::VCPU_GENERAL_OFFSET as usize);
    assert!(offset_of!(VcpuContext, program_counter) == registers::VCPU_PC_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vsstatus) == registers::VCPU_VSSTATUS_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vsie) == registers::VCPU_VSIE_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vstvec) == registers::VCPU_VSTVEC_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vsscratch) == registers::VCPU_VSSCRATCH_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vsepc) == registers::VCPU_VSEPC_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vscause) == registers::VCPU_VSCAUSE_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vstval) == registers::VCPU_VSTVAL_OFFSET as usize);
    assert!(offset_of!(VcpuContext, hvip) == registers::VCPU_HVIP_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vsatp) == registers::VCPU_VSATP_OFFSET as usize);
    assert!(offset_of!(VcpuContext, vstimecmp) == registers::VCPU_VSTIMECMP_OFFSET as usize);
    assert!(offset_of!(VcpuContext, scounteren) == registers::VCPU_SCOUNTEREN_OFFSET as usize);
    assert!(offset_of!(VcpuContext, senvcfg) == registers::VCPU_SENVCFG_OFFSET as usize);
    assert!(
        offset_of!(VcpuContext, virtual_count_offset)
            == registers::VCPU_VIRTUAL_COUNT_OFFSET as usize
    );
    assert!(offset_of!(VcpuContext, floating) == registers::VCPU_FLOATING_OFFSET as usize);
    assert!(offset_of!(VcpuContext, fcsr) == registers::VCPU_FCSR_OFFSET as usize);
    assert!(offset_of!(VcpuContext, run_state) == registers::VCPU_RUN_STATE_OFFSET as usize);
    assert!(offset_of!(VcpuContext, supervisor) == registers::VCPU_SUPERVISOR_OFFSET as usize);
    assert!(size_of::<GuestAnchorExit>() == 16);
};
