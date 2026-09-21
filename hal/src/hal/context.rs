// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected schedulable-context and stack-transition capabilities.
//!
//! Task policy owns thread lifecycle, stacks, and pinning. This facade selects
//! the architecture register image and the machine transitions operating on
//! that scheduler-owned state.

pub use crate::arch::context::SwitchCompletion;
pub use crate::arch::context::ThreadContext;

/// Switches between two pinned scheduler-owned machine contexts.
///
/// # Safety
///
/// Both pointers must be non-null, aligned scheduler-owned contexts whose
/// stacks remain pinned until control eventually switches back. `previous`
/// must be uniquely writable and `next` must contain a prepared context. No
/// Rust reference may remain live because `completion` re-enters the scheduler.
/// Local interrupts must be masked. The outgoing interrupt state must come
/// from the consumed transition guard, and `completion` must neither block nor
/// switch while it runs on the incoming stack before interrupt restoration.
#[inline]
pub unsafe fn switch_thread_context(
    previous: *mut ThreadContext,
    next: *const ThreadContext,
    previous_interrupt_state: <crate::hal::irq::LocalMask as hyper::hal::interrupt::InterruptMask>::State,
    completion: SwitchCompletion,
    completion_ticket: usize,
) {
    // SAFETY: The selected architecture facade receives the same pinned,
    // exclusively scheduler-owned contexts and prepared-state guarantee.
    unsafe {
        crate::arch::context::switch_thread_context(
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
pub unsafe fn reset_stack_and_enter(
    bottom: usize,
    top: usize,
    watermark: u64,
    canary: u64,
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The selected architecture facade receives the same stack range,
    // lifetime, mask-state, and non-returning callback guarantees.
    unsafe {
        crate::arch::context::reset_stack_and_enter(
            bottom, top, watermark, canary, callback, argument,
        )
    }
}
