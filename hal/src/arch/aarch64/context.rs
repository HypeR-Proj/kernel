// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::mem::{offset_of, size_of};

use super::registers;
use super::user_contract::LowerElReturnRegime;

pub type KernelThreadEntry = extern "C" fn(usize);

#[repr(C, align(16))]
pub struct ThreadContext {
    callee_saved: [u64; 10],
    frame_pointer: u64,
    link_register: u64,
    stack_pointer: u64,
    simd_callee_saved: [u64; 8],
    fpcr: u64,
    fpsr: u64,
    interrupt_mask: u64,
}

impl ThreadContext {
    pub const fn empty() -> Self {
        Self {
            callee_saved: [0; 10],
            frame_pointer: 0,
            link_register: 0,
            stack_pointer: 0,
            simd_callee_saved: [0; 8],
            fpcr: 0,
            fpsr: 0,
            // Runtime kernel Threads begin with IRQ enabled while debug,
            // SError, and FIQ remain masked, matching enable_irq().
            interrupt_mask: registers::SPSR_D | registers::SPSR_A | registers::SPSR_F,
        }
    }

    pub fn prepare(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.callee_saved[0] = entry as usize as u64;
        self.callee_saved[1] = argument as u64;
        self.link_register = aarch64_thread_trampoline as *const () as usize as u64;
        self.stack_pointer = (stack_top & !(registers::STACK_ALIGNMENT_MASK as usize)) as u64;
    }

    /// Prepares a vCPU bootstrap continuation with IRQ masked.
    ///
    /// The scheduler publishes the vCPU as current before its trampoline can
    /// publish active virtual hardware. Keeping IRQ masked closes that first-
    /// run ownership gap; the vCPU run loop controls guest-entry unmasking.
    pub fn prepare_vcpu(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.prepare(stack_top, entry, argument);
        self.interrupt_mask |= registers::SPSR_I;
    }
}

#[repr(C, align(16))]
pub struct GuestSimdContext([[u64; 2]; 32]);

#[repr(C, align(16))]
pub struct VcpuContext {
    pub(crate) general: [u64; 31],
    pub(crate) stack_pointer_el0: u64,
    pub(crate) stack_pointer_el1: u64,
    pub(crate) program_counter: u64,
    pub(crate) processor_state: u64,
    pub(crate) simd: GuestSimdContext,
    pub(crate) fpcr: u64,
    pub(crate) fpsr: u64,
    pub(crate) sctlr_el1: u64,
    pub(crate) tcr_el1: u64,
    pub(crate) ttbr0_el1: u64,
    pub(crate) ttbr1_el1: u64,
    pub(crate) mair_el1: u64,
    pub(crate) amair_el1: u64,
    pub(crate) vbar_el1: u64,
    pub(crate) cpacr_el1: u64,
    pub(crate) cntkctl_el1: u64,
    pub(crate) afsr0_el1: u64,
    pub(crate) afsr1_el1: u64,
    pub(crate) esr_el1: u64,
    pub(crate) far_el1: u64,
    pub(crate) par_el1: u64,
    pub(crate) elr_el1: u64,
    pub(crate) spsr_el1: u64,
    pub(crate) contextidr_el1: u64,
    pub(crate) tpidr_el0: u64,
    pub(crate) tpidrro_el0: u64,
    pub(crate) tpidr_el1: u64,
    timer: super::timer::VirtualTimerContext,
    pub(crate) vgic: super::VgicCpuContext,
    run_state: u64,
    terminal_kind: u64,
    terminal_syndrome: u64,
    terminal_fault_address: u64,
    terminal_vector: u64,
    terminal_synchronous: Option<GuestSynchronousTerminal>,
    deferred_device: Option<super::vsysreg::GuestDeviceCompletion>,
    vgic_bank: super::vgic::bank::Bank,
}

const GUEST_RUN_READY: u64 = 1;
const GUEST_RUN_RUNNING: u64 = 2;
const GUEST_RUN_STOPPED: u64 = 3;

const TERMINAL_MEMORY_FAULT: u64 = 1;
const TERMINAL_MMIO: u64 = 2;
const TERMINAL_SYNCHRONOUS: u64 = 3;
const WAIT_FOR_INTERRUPT: u64 = 4;
const ADMINISTRATIVE_STOP: u64 = 5;
const WAIT_FOR_DEVICE: u64 = 6;

/// Decoded detail for a synchronous exit which cannot resume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestSynchronousTerminal {
    Undecodable,
    Failed {
        exit: super::vsysreg::GuestSyncExit,
        failure: super::vsysreg::GuestSyncFailure,
    },
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
    Device,
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

    fn consume_for(&mut self, context: &mut VcpuContext) -> Result<(), GuestRunError> {
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
                format_args!("armed AArch64 stopped-guest proof was dropped without detachment"),
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
        Self {
            general: [0; 31],
            stack_pointer_el0: 0,
            stack_pointer_el1: 0,
            program_counter,
            processor_state: registers::SPSR_EL1H_AND_DAIF,
            simd: GuestSimdContext([[0; 2]; 32]),
            fpcr: 0,
            fpsr: 0,
            sctlr_el1: registers::SCTLR_EL1_GUEST_RESET_VALUE,
            tcr_el1: 0,
            ttbr0_el1: 0,
            ttbr1_el1: 0,
            mair_el1: 0,
            amair_el1: 0,
            vbar_el1: 0,
            cpacr_el1: 0,
            cntkctl_el1: 0,
            afsr0_el1: 0,
            afsr1_el1: 0,
            esr_el1: 0,
            far_el1: 0,
            par_el1: 0,
            elr_el1: 0,
            spsr_el1: 0,
            contextidr_el1: 0,
            tpidr_el0: 0,
            tpidrro_el0: 0,
            tpidr_el1: 0,
            timer: super::timer::VirtualTimerContext::empty(),
            vgic: super::VgicCpuContext::empty(),
            run_state: GUEST_RUN_READY,
            terminal_kind: 0,
            terminal_syndrome: 0,
            terminal_fault_address: 0,
            terminal_vector: 0,
            terminal_synchronous: None,
            deferred_device: None,
            vgic_bank: super::vgic::bank::Bank::new(),
        }
    }

    /// Prepares the hardware-assisted virtual interrupt interface state.
    pub(crate) fn initialize_virtual_interrupts(
        &mut self,
    ) -> Result<super::VgicCapabilities, super::VgicError> {
        super::vgic::initialize_context(&mut self.vgic)
    }

    /// Sets the guest-visible virtual count to `value` at the supplied
    /// physical counter instant.
    pub(crate) fn set_virtual_count(&mut self, physical_count: u64, value: u64) {
        self.timer.set_offset(physical_count.wrapping_sub(value));
    }

    pub(crate) fn set_virtual_timer_deadline(&mut self, deadline: u64) {
        self.timer.set_compare_value(deadline);
    }

    pub(crate) fn set_virtual_timer_enabled(&mut self, enabled: bool) {
        self.timer.set_enabled(enabled);
    }

    pub(crate) fn virtual_timer_interrupt_asserted_at(&self, physical_count: u64) -> bool {
        self.timer.interrupt_asserted_at(physical_count)
    }

    pub(crate) fn virtual_timer_wfi_wake_at(
        &self,
        physical_count: u64,
    ) -> hyper::drivers::timer::arm_generic::VirtualTimerWake {
        self.timer.wfi_wake_at(physical_count)
    }

    pub(crate) fn virtual_timer_interrupt_asserted_hardware(&self) -> bool {
        super::timer::virtual_timer_interrupt_asserted()
    }

    /// Loads the guest-owned EL1 system-register bank on the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must exclusively own this stopped vCPU, and lower-EL guest
    /// execution must remain disabled until the rest of its state is loaded.
    pub(crate) unsafe fn activate_system_registers(&self) {
        // EL1 register names select the VHE host bank. EL12 aliases are the
        // only safe way to load the guest bank without replacing host state.
        // SAFETY: The caller exclusively owns the inactive guest context; the
        // final ISB completes the EL12 state transition before guest entry.
        unsafe {
            asm!(
                "msr S3_5_C2_C0_2, {tcr}",
                "msr S3_5_C2_C0_0, {ttbr0}",
                "msr S3_5_C2_C0_1, {ttbr1}",
                "msr S3_5_C10_C2_0, {mair}",
                "msr S3_5_C10_C3_0, {amair}",
                "msr S3_5_C1_C0_0, {sctlr}",
                "msr S3_5_C12_C0_0, {vbar}",
                "msr S3_5_C1_C0_2, {cpacr}",
                "msr S3_5_C14_C1_0, {cntkctl}",
                "msr S3_5_C5_C1_0, {afsr0}",
                "msr S3_5_C5_C1_1, {afsr1}",
                "msr S3_5_C13_C0_1, {contextidr}",
                "msr TPIDR_EL0, {tpidr_el0}",
                "msr TPIDRRO_EL0, {tpidrro_el0}",
                "msr TPIDR_EL1, {tpidr_el1}",
                "msr S3_5_C5_C2_0, {esr}",
                "msr S3_5_C6_C0_0, {far}",
                "msr PAR_EL1, {par}",
                "msr S3_5_C4_C0_1, {elr}",
                "msr S3_5_C4_C0_0, {spsr}",
                "msr SP_EL0, {sp_el0}",
                "msr SP_EL1, {sp_el1}",
                "isb",
                sctlr = in(reg) self.sctlr_el1,
                tcr = in(reg) self.tcr_el1,
                ttbr0 = in(reg) self.ttbr0_el1,
                ttbr1 = in(reg) self.ttbr1_el1,
                mair = in(reg) self.mair_el1,
                amair = in(reg) self.amair_el1,
                vbar = in(reg) self.vbar_el1,
                cpacr = in(reg) self.cpacr_el1,
                cntkctl = in(reg) self.cntkctl_el1,
                afsr0 = in(reg) self.afsr0_el1,
                afsr1 = in(reg) self.afsr1_el1,
                contextidr = in(reg) self.contextidr_el1,
                tpidr_el0 = in(reg) self.tpidr_el0,
                tpidrro_el0 = in(reg) self.tpidrro_el0,
                tpidr_el1 = in(reg) self.tpidr_el1,
                esr = in(reg) self.esr_el1,
                far = in(reg) self.far_el1,
                par = in(reg) self.par_el1,
                elr = in(reg) self.elr_el1,
                spsr = in(reg) self.spsr_el1,
                sp_el0 = in(reg) self.stack_pointer_el0,
                sp_el1 = in(reg) self.stack_pointer_el1,
                options(nostack, preserves_flags)
            );
        }
    }

    /// Saves the live guest-owned EL1 system-register bank.
    ///
    /// # Safety
    ///
    /// This context must be the vCPU currently loaded on the calling CPU, and
    /// guest execution must already have stopped with local IRQs masked.
    pub(crate) unsafe fn deactivate_system_registers(&mut self) {
        // SAFETY: Guest execution is stopped and the caller exclusively owns
        // both this context and the live EL12 bank being sampled.
        unsafe {
            asm!(
                "mrs {sctlr}, S3_5_C1_C0_0",
                "mrs {tcr}, S3_5_C2_C0_2",
                "mrs {ttbr0}, S3_5_C2_C0_0",
                "mrs {ttbr1}, S3_5_C2_C0_1",
                "mrs {mair}, S3_5_C10_C2_0",
                "mrs {amair}, S3_5_C10_C3_0",
                "mrs {vbar}, S3_5_C12_C0_0",
                "mrs {cpacr}, S3_5_C1_C0_2",
                "mrs {cntkctl}, S3_5_C14_C1_0",
                "mrs {afsr0}, S3_5_C5_C1_0",
                "mrs {afsr1}, S3_5_C5_C1_1",
                "mrs {contextidr}, S3_5_C13_C0_1",
                "mrs {tpidr_el0}, TPIDR_EL0",
                "mrs {tpidrro_el0}, TPIDRRO_EL0",
                "mrs {tpidr_el1}, TPIDR_EL1",
                "mrs {esr}, S3_5_C5_C2_0",
                "mrs {far}, S3_5_C6_C0_0",
                "mrs {par}, PAR_EL1",
                "mrs {elr}, S3_5_C4_C0_1",
                "mrs {spsr}, S3_5_C4_C0_0",
                "mrs {sp_el0}, SP_EL0",
                "mrs {sp_el1}, SP_EL1",
                sctlr = out(reg) self.sctlr_el1,
                tcr = out(reg) self.tcr_el1,
                ttbr0 = out(reg) self.ttbr0_el1,
                ttbr1 = out(reg) self.ttbr1_el1,
                mair = out(reg) self.mair_el1,
                amair = out(reg) self.amair_el1,
                vbar = out(reg) self.vbar_el1,
                cpacr = out(reg) self.cpacr_el1,
                cntkctl = out(reg) self.cntkctl_el1,
                afsr0 = out(reg) self.afsr0_el1,
                afsr1 = out(reg) self.afsr1_el1,
                contextidr = out(reg) self.contextidr_el1,
                tpidr_el0 = out(reg) self.tpidr_el0,
                tpidrro_el0 = out(reg) self.tpidrro_el0,
                tpidr_el1 = out(reg) self.tpidr_el1,
                esr = out(reg) self.esr_el1,
                far = out(reg) self.far_el1,
                par = out(reg) self.par_el1,
                elr = out(reg) self.elr_el1,
                spsr = out(reg) self.spsr_el1,
                sp_el0 = out(reg) self.stack_pointer_el0,
                sp_el1 = out(reg) self.stack_pointer_el1,
                options(nostack, preserves_flags)
            );
        }
    }

    pub(crate) fn take_reused_vgic_save(&mut self) -> bool {
        self.vgic_bank.take_reused_save()
    }

    /// Loads this vCPU's GIC virtualization state on the current CPU.
    ///
    /// # Safety
    ///
    /// No other CPU may run this vCPU, and guest execution must not already be
    /// active on the calling CPU.
    pub(crate) unsafe fn activate_vgic(&mut self) -> Result<(), super::VgicError> {
        self.vgic_bank.load(super::VgicError::StateMismatch, || {
            // SAFETY: The caller owns the local bank; the guard rejects double loads.
            unsafe { super::vgic::activate(&self.vgic) }
        })
    }

    /// Saves this vCPU's GIC virtualization state and disables guest delivery.
    ///
    /// # Safety
    ///
    /// The caller must retain exclusive ownership of this CPU bank, with local
    /// IRQs masked. It may already have been saved by a local operation; no
    /// other vCPU may have acquired the bank in between.
    pub(crate) unsafe fn deactivate_vgic(&mut self) -> Result<(), super::VgicError> {
        self.vgic_bank.save(super::VgicError::StateMismatch, || {
            // SAFETY: The caller owns this bank. Already-saved cleanup never reads it.
            unsafe { super::vgic::deactivate(&mut self.vgic) }
        })
    }

    /// Loads this vCPU's architectural virtual timer state locally.
    ///
    /// # Safety
    ///
    /// No other CPU may run this vCPU, and guest execution must not already be
    /// active on the calling CPU.
    pub(crate) unsafe fn activate_timer(&self) {
        // SAFETY: This method forwards its stopped, exclusively owned vCPU
        // contract to the local timer backend.
        unsafe { super::timer::activate_virtual_timer(&self.timer) };
    }

    /// Saves and disables this vCPU's architectural virtual timer locally.
    ///
    /// # Safety
    ///
    /// Local IRQs must be masked and this must be the active local vCPU.
    pub(crate) unsafe fn deactivate_timer(&mut self) {
        // SAFETY: This method forwards its active-vCPU and masked-IRQ contract.
        unsafe { super::timer::deactivate_virtual_timer(&mut self.timer) };
    }

    /// Runs the active guest until vector entry captures a typed stopped exit.
    ///
    /// # Safety
    ///
    /// `context` must be non-null, aligned, pinned, and exclusively owned by
    /// the active vCPU. Stage-2 translation and every guest-owned architectural
    /// context must be active on this CPU. Local IRQs must remain masked until
    /// the assembly path executes `ERET`. No Rust reference to the context may
    /// remain live: exception reentry mutates it before this call can return.
    pub(crate) unsafe fn run(context: *mut Self) -> Result<StoppedGuestRun, GuestRunError> {
        if context.is_null() || !context.is_aligned() {
            return Err(GuestRunError::Owner);
        }
        // SAFETY: The caller provides this pinned, exclusively owned context.
        let context_ref = unsafe { &mut *context };
        if context_ref.run_state != GUEST_RUN_READY || context_ref.deferred_device.is_some() {
            return Err(GuestRunError::State);
        }
        context_ref.run_state = GUEST_RUN_RUNNING;
        let entry_hcr = LowerElReturnRegime::guest_hcr();
        // The exclusive reference ends before assembly transfers to the guest.
        // SAFETY: The caller established the complete active guest contract;
        // assembly retains the only live machine-context access until unwind.
        let raw = unsafe { aarch64_run_guest(context.cast::<u8>(), entry_hcr) };
        // SAFETY: Assembly returns only after vector capture has closed its
        // return-world publication and released every context borrow.
        let context_ref = unsafe { &mut *context };
        if raw != registers::GUEST_RUN_RETURN_STOPPED || context_ref.run_state != GUEST_RUN_STOPPED
        {
            return Err(GuestRunError::Return);
        }
        let exit = match context_ref.terminal_kind {
            TERMINAL_MEMORY_FAULT | TERMINAL_MMIO | TERMINAL_SYNCHRONOUS => {
                let cause = match context_ref.terminal_kind {
                    TERMINAL_MEMORY_FAULT => GuestTerminalCause::MemoryFault,
                    TERMINAL_MMIO => GuestTerminalCause::Mmio,
                    TERMINAL_SYNCHRONOUS => GuestTerminalCause::Synchronous(
                        context_ref
                            .terminal_synchronous
                            .ok_or(GuestRunError::Return)?,
                    ),
                    _ => return Err(GuestRunError::Return),
                };
                GuestRunExit::Terminal(GuestTerminalExit {
                    cause,
                    syndrome: context_ref.terminal_syndrome,
                    fault_address: context_ref.terminal_fault_address,
                    program_counter: context_ref.program_counter,
                    processor_state: context_ref.processor_state,
                    vector: context_ref.terminal_vector,
                })
            }
            WAIT_FOR_INTERRUPT => GuestRunExit::Wait(GuestWaitReason::Interrupt),
            WAIT_FOR_DEVICE => GuestRunExit::Wait(GuestWaitReason::Device),
            ADMINISTRATIVE_STOP => {
                GuestRunExit::AdministrativeStop(GuestAdministrativeStopReason::Requested)
            }
            _ => return Err(GuestRunError::Return),
        };
        Ok(StoppedGuestRun {
            context,
            exit,
            armed: true,
            not_send_or_sync: core::marker::PhantomData,
        })
    }

    pub(super) fn capture_terminal(
        &mut self,
        frame: &super::exception::ExceptionFrame,
        cause: GuestTerminalCause,
    ) -> Result<(), GuestRunError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestRunError::State);
        }
        self.general = frame.general;
        self.stack_pointer_el0 = frame.sp_el0;
        self.stack_pointer_el1 = frame.sp_el1;
        self.program_counter = frame.elr;
        self.processor_state = frame.spsr;
        self.simd.0 = frame.simd;
        self.fpcr = frame.fpcr;
        self.fpsr = frame.fpsr;
        self.terminal_kind = match cause {
            GuestTerminalCause::MemoryFault => TERMINAL_MEMORY_FAULT,
            GuestTerminalCause::Mmio => TERMINAL_MMIO,
            GuestTerminalCause::Synchronous(_) => TERMINAL_SYNCHRONOUS,
        };
        self.terminal_synchronous = match cause {
            GuestTerminalCause::Synchronous(synchronous) => Some(synchronous),
            GuestTerminalCause::MemoryFault | GuestTerminalCause::Mmio => None,
        };
        self.terminal_syndrome = frame.esr;
        self.terminal_fault_address = frame.far;
        self.terminal_vector = frame.vector;
        self.run_state = GUEST_RUN_STOPPED;
        Ok(())
    }

    pub(super) fn capture_wait(
        &mut self,
        frame: &super::exception::ExceptionFrame,
    ) -> Result<(), GuestRunError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestRunError::State);
        }
        self.general = frame.general;
        self.stack_pointer_el0 = frame.sp_el0;
        self.stack_pointer_el1 = frame.sp_el1;
        self.program_counter = frame.elr;
        self.processor_state = frame.spsr;
        self.simd.0 = frame.simd;
        self.fpcr = frame.fpcr;
        self.fpsr = frame.fpsr;
        self.terminal_kind = WAIT_FOR_INTERRUPT;
        self.terminal_syndrome = frame.esr;
        self.terminal_fault_address = frame.far;
        self.terminal_vector = frame.vector;
        self.terminal_synchronous = None;
        self.run_state = GUEST_RUN_STOPPED;
        Ok(())
    }

    pub(super) fn capture_device(
        &mut self,
        frame: &super::exception::ExceptionFrame,
        completion: super::vsysreg::GuestDeviceCompletion,
    ) -> Result<(), GuestRunError> {
        if self.deferred_device.is_some() {
            return Err(GuestRunError::State);
        }
        self.capture_wait(frame)?;
        self.deferred_device = Some(completion);
        self.terminal_kind = WAIT_FOR_DEVICE;
        Ok(())
    }

    /// Completes the saved instruction only while hardware is detached.
    pub(crate) fn complete_device(
        &mut self,
        action: hyper::vm::exit::MmioAction,
    ) -> Result<(), GuestRunError> {
        if self.run_state != GUEST_RUN_READY {
            return Err(GuestRunError::State);
        }
        let completion = self.deferred_device.ok_or(GuestRunError::State)?;
        if !completion.apply(&mut self.general, &mut self.program_counter, action) {
            return Err(GuestRunError::State);
        }
        self.deferred_device = None;
        Ok(())
    }

    pub(super) fn capture_administrative_stop(
        &mut self,
        frame: &super::exception::ExceptionFrame,
    ) -> Result<(), GuestRunError> {
        if self.run_state != GUEST_RUN_RUNNING {
            return Err(GuestRunError::State);
        }
        self.general = frame.general;
        self.stack_pointer_el0 = frame.sp_el0;
        self.stack_pointer_el1 = frame.sp_el1;
        self.program_counter = frame.elr;
        self.processor_state = frame.spsr;
        self.simd.0 = frame.simd;
        self.fpcr = frame.fpcr;
        self.fpsr = frame.fpsr;
        self.terminal_kind = ADMINISTRATIVE_STOP;
        self.terminal_syndrome = frame.esr;
        self.terminal_fault_address = frame.far;
        self.terminal_vector = frame.vector;
        self.terminal_synchronous = None;
        self.run_state = GUEST_RUN_STOPPED;
        Ok(())
    }

    pub(super) fn consume_stopped(
        &mut self,
        stopped: &mut StoppedGuestRun,
    ) -> Result<(), GuestRunError> {
        stopped.consume_for(self)
    }
}

unsafe extern "C" {
    fn aarch64_switch_context(
        previous: *mut ThreadContext,
        next: *const ThreadContext,
        previous_interrupt_state: u64,
        completion: extern "C" fn(usize),
        completion_ticket: usize,
    );
    fn aarch64_thread_trampoline();
    fn aarch64_run_guest(context: *mut u8, entry_hcr: u64) -> u64;
    fn aarch64_reset_stack_and_enter(
        bottom: usize,
        top: usize,
        watermark: u64,
        canary: u64,
        callback: extern "C" fn(usize) -> !,
        argument: usize,
    ) -> !;
    fn aarch64_run_on_emergency_stack(callback: extern "C" fn(usize) -> !, argument: usize) -> !;
}

/// Switches AAPCS64 callee-saved state and kernel stacks.
///
/// # Safety
///
/// Both pointers must be valid pinned scheduler contexts, `previous` must be
/// uniquely writable, and `next` must own a valid mapped kernel stack. No Rust
/// reference may remain live because `completion` re-enters scheduler ownership.
/// Local exceptions must be masked; the callback must neither block nor switch.
pub unsafe fn switch_thread_context(
    previous: *mut ThreadContext,
    next: *const ThreadContext,
    previous_interrupt_state: u64,
    completion: extern "C" fn(usize),
    completion_ticket: usize,
) {
    // SAFETY: The caller pins both contexts and guarantees `next` owns a valid
    // mapped stack until control eventually switches back.
    unsafe {
        aarch64_switch_context(
            previous,
            next,
            previous_interrupt_state,
            completion,
            completion_ticket,
        )
    };
}

/// Abandons the current call chain and enters a continuation on a clean stack.
///
/// # Safety
///
/// `bottom..top` must be the exclusively owned, writable stack currently in
/// use. `bottom` must be 8-byte aligned, `top` must be 16-byte aligned, and the
/// nonempty range length must be a multiple of 8. Interrupts must be masked,
/// and `callback` must never return.
pub unsafe fn reset_stack_and_enter(
    bottom: usize,
    top: usize,
    watermark: u64,
    canary: u64,
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The caller supplies the exclusive aligned stack range and a
    // non-returning callback required by the assembly ABI.
    unsafe { aarch64_reset_stack_and_enter(bottom, top, watermark, canary, callback, argument) }
}

/// Permanently invokes fatal handling on the calling CPU's emergency stack.
///
/// # Safety
///
/// `argument` must remain valid forever or until `callback` stops the CPU.
pub unsafe fn run_on_emergency_stack(callback: extern "C" fn(usize) -> !, argument: usize) -> ! {
    // SAFETY: The caller guarantees the callback argument remains valid for
    // this irreversible transfer to the pinned emergency stack.
    unsafe { aarch64_run_on_emergency_stack(callback, argument) }
}

const _: () = {
    assert!(
        offset_of!(ThreadContext, callee_saved) == registers::THREAD_CONTEXT_X19_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, frame_pointer) == registers::THREAD_CONTEXT_X29_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, link_register) == registers::THREAD_CONTEXT_X30_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, stack_pointer) == registers::THREAD_CONTEXT_SP_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, simd_callee_saved)
            == registers::THREAD_CONTEXT_D8_OFFSET as usize
    );
    assert!(offset_of!(ThreadContext, fpcr) == registers::THREAD_CONTEXT_FPCR_OFFSET as usize);
    assert!(offset_of!(ThreadContext, fpsr) == registers::THREAD_CONTEXT_FPSR_OFFSET as usize);
    assert!(
        offset_of!(ThreadContext, interrupt_mask) == registers::THREAD_CONTEXT_DAIF_OFFSET as usize
    );
    assert!(size_of::<ThreadContext>() == 192);
    assert!(offset_of!(VcpuContext, general) == registers::VCPU_CONTEXT_X0_OFFSET as usize);
    assert!(
        offset_of!(VcpuContext, general) + 30 * size_of::<u64>()
            == registers::VCPU_CONTEXT_X30_OFFSET as usize
    );
    assert!(offset_of!(VcpuContext, program_counter) == registers::VCPU_CONTEXT_PC_OFFSET as usize);
    assert!(
        offset_of!(VcpuContext, processor_state) == registers::VCPU_CONTEXT_PSTATE_OFFSET as usize
    );
    assert!(offset_of!(VcpuContext, simd) == registers::VCPU_CONTEXT_SIMD_OFFSET as usize);
    assert!(offset_of!(VcpuContext, fpcr) == registers::VCPU_CONTEXT_FPCR_OFFSET as usize);
    assert!(offset_of!(VcpuContext, fpsr) == registers::VCPU_CONTEXT_FPSR_OFFSET as usize);
};
