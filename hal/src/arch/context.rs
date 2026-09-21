// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture schedulable context and stack-transition mechanisms.
//!
//! Task policy owns thread lifecycle and scheduling. This facade owns only the
//! architecture register images and the machine operations which initialize or
//! switch them. Stack ownership and mapping remain kernel memory policy.

pub use super::imp::ThreadContext;

pub type SwitchCompletion = extern "C" fn(usize);

/// Switches between two pinned scheduler-owned machine contexts.
///
/// # Safety
///
/// Both pointers must be non-null, aligned scheduler-owned contexts whose
/// associated stacks remain pinned until control eventually switches back.
/// `previous` must be uniquely writable and `next` must contain a context
/// prepared for the selected architecture. No Rust reference may remain live
/// across this call because `completion` re-enters scheduler ownership.
/// Local interrupts must be masked. `previous_interrupt_state` must be the
/// exact state consumed from the outgoing transition guard. `completion` must
/// not block or switch; it runs on the incoming stack before that context's
/// interrupt state is restored.
#[inline]
pub(crate) unsafe fn switch_thread_context(
    previous: *mut ThreadContext,
    next: *const ThreadContext,
    previous_interrupt_state: <super::irq::LocalMask as hyper::hal::interrupt::InterruptMask>::State,
    completion: SwitchCompletion,
    completion_ticket: usize,
) {
    // SAFETY: This facade preserves the selected backend's context ownership,
    // pinning, and prepared-state preconditions unchanged.
    unsafe {
        super::imp::switch_thread_context(
            previous,
            next,
            previous_interrupt_state,
            completion,
            completion_ticket,
        )
    }
}

/// Abandons the current call chain and enters a continuation on a clean stack.
///
/// # Safety
///
/// `bottom..top` must be a nonempty, exclusively owned, writable stack range
/// satisfying the selected architecture's ABI alignment. Local interrupts
/// must be masked, no live value on the abandoned stack may be used again, and
/// `callback` must not return.
#[inline]
pub(crate) unsafe fn reset_stack_and_enter(
    bottom: usize,
    top: usize,
    watermark: u64,
    canary: u64,
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: This facade forwards every backend stack, alignment, lifetime,
    // interrupt-state, and non-returning callback precondition.
    unsafe { super::imp::reset_stack_and_enter(bottom, top, watermark, canary, callback, argument) }
}
