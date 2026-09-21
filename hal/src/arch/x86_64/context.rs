// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::mem::{offset_of, size_of};

use super::registers;

pub type KernelThreadEntry = extern "C" fn(usize);

#[repr(C, align(16))]
pub struct ThreadContext {
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    stack_pointer: u64,
    instruction_pointer: u64,
    interrupt_enable: u64,
}

impl ThreadContext {
    pub const fn empty() -> Self {
        Self {
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            stack_pointer: 0,
            instruction_pointer: 0,
            interrupt_enable: 1 << 9,
        }
    }

    pub fn prepare(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.r12 = entry as usize as u64;
        self.r13 = argument as u64;
        // Context-switch completion uses the incoming stack before the fresh
        // trampoline. Match a normal SysV function-entry stack so that the
        // assembly tail can issue an aligned call for both fresh and resumed
        // contexts.
        self.stack_pointer = ((stack_top & !15) - 8) as u64;
        self.instruction_pointer = x86_64_thread_trampoline as *const () as usize as u64;
    }

    pub fn prepare_vcpu(&mut self, stack_top: usize, entry: KernelThreadEntry, argument: usize) {
        self.prepare(stack_top, entry, argument);
        self.interrupt_enable = 0;
    }
}

#[repr(C, align(16))]
pub struct VcpuContext {
    pub(crate) general: [u64; 16],
    pub(crate) instruction_pointer: u64,
    pub(crate) flags: u64,
    pub(super) msrs: VcpuMsrState,
    pub(super) fx_state: VcpuFxState,
}

#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub(super) struct VcpuFxState([u8; 512]);

impl VcpuFxState {
    const fn initial() -> Self {
        let mut state = [0; 512];
        // Architectural x87 control word and MXCSR reset values in FXSAVE
        // layout. All x87 tags are empty and all data registers are zero.
        state[0] = 0x7f;
        state[1] = 0x03;
        state[24] = 0x80;
        state[25] = 0x1f;
        Self(state)
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub(super) struct VcpuMsrState {
    pub star: u64,
    pub lstar: u64,
    pub cstar: u64,
    pub sfmask: u64,
    pub kernel_gs_base: u64,
    pub tsc_aux: u64,
}

impl VcpuContext {
    pub const fn new(instruction_pointer: u64) -> Self {
        Self {
            general: [0; 16],
            instruction_pointer,
            flags: 2,
            msrs: VcpuMsrState {
                star: 0,
                lstar: 0,
                cstar: 0,
                sfmask: 0,
                kernel_gs_base: 0,
                tsc_aux: 0,
            },
            fx_state: VcpuFxState::initial(),
        }
    }

    pub(crate) fn set_virtual_count(&mut self, _guest_count: u64, _host_count: u64) {}

    pub(crate) fn initialize_virtual_interrupts(&mut self) -> Result<(), VirtualInterruptError> {
        Ok(())
    }

    /// Enters the guest represented by `context`.
    ///
    /// # Safety
    ///
    /// `context` must be non-null, aligned, pinned, and exclusively owned by the
    /// active vCPU for the guest-run lifetime. VM exit may mutate it.
    pub(crate) unsafe fn enter(context: *mut Self) -> ! {
        // SAFETY: The caller establishes the raw context and hardware-state contract.
        unsafe { super::virtualization::enter(context) }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtualInterruptError {
    NotInitialized,
}

unsafe extern "C" {
    fn x86_64_switch_context(
        previous: *mut ThreadContext,
        next: *const ThreadContext,
        previous_interrupt_state: usize,
        completion: extern "C" fn(usize),
        completion_ticket: usize,
    );
    fn x86_64_thread_trampoline();
    fn x86_64_reset_stack_and_enter(
        bottom: usize,
        top: usize,
        watermark: u64,
        canary: u64,
        callback: extern "C" fn(usize) -> !,
        argument: usize,
    ) -> !;
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
        x86_64_switch_context(
            previous,
            next,
            previous_interrupt_state,
            completion,
            completion_ticket,
        )
    };
}

/// Restores the timer side of the local interrupt-enable contract.
#[unsafe(no_mangle)]
extern "C" fn x86_64_prepare_context_interrupt_enable() {
    super::timer::prepare_interrupt_enable();
}

/// Resets a stack and transfers control to `callback`.
///
/// # Safety
///
/// `[bottom, top)` must be exclusively owned, writable stack memory. `top`
/// must meet the x86-64 System V alignment requirement, and callback must not return.
pub unsafe fn reset_stack_and_enter(
    bottom: usize,
    top: usize,
    watermark: u64,
    canary: u64,
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The caller provides an exclusive valid stack and non-returning callback.
    unsafe { x86_64_reset_stack_and_enter(bottom, top, watermark, canary, callback, argument) }
}

const _: () = {
    assert!(offset_of!(ThreadContext, rbx) == registers::THREAD_CONTEXT_RBX_OFFSET as usize);
    assert!(offset_of!(ThreadContext, rbp) == registers::THREAD_CONTEXT_RBP_OFFSET as usize);
    assert!(offset_of!(ThreadContext, r12) == registers::THREAD_CONTEXT_R12_OFFSET as usize);
    assert!(offset_of!(ThreadContext, r13) == registers::THREAD_CONTEXT_R13_OFFSET as usize);
    assert!(offset_of!(ThreadContext, r14) == registers::THREAD_CONTEXT_R14_OFFSET as usize);
    assert!(offset_of!(ThreadContext, r15) == registers::THREAD_CONTEXT_R15_OFFSET as usize);
    assert!(
        offset_of!(ThreadContext, stack_pointer) == registers::THREAD_CONTEXT_RSP_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, instruction_pointer)
            == registers::THREAD_CONTEXT_RIP_OFFSET as usize
    );
    assert!(
        offset_of!(ThreadContext, interrupt_enable) == registers::THREAD_CONTEXT_IF_OFFSET as usize
    );
    assert!(size_of::<ThreadContext>() == registers::THREAD_CONTEXT_SIZE as usize);
    assert!(offset_of!(VcpuContext, fx_state) == registers::VCPU_CONTEXT_FX_STATE_OFFSET as usize);
};
