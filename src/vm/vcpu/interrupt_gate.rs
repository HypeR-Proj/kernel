// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler waiting for a VM's short interrupt-state transaction.

/// Returns when entry can be retried or administrative stop needs handling.
///
/// # Safety
///
/// The scheduler must exclusively own and pin `execution` as `thread`, with
/// hardware detached and execution claims released. Local IRQs are masked by
/// the Thread context, not by a CPU-affine guard spanning this wait.
pub(crate) unsafe fn wait_for_interrupt_gate(
    execution: *mut super::VcpuExecution,
    thread: crate::task::thread::ThreadId,
) {
    loop {
        // SAFETY: The caller retains the pinned, hardware-detached execution.
        let current = unsafe { &*execution };
        let Some(binding) = current.vm_binding() else {
            hyper::debug::invariant_failure("interrupt gate wait lost VM binding");
        };
        let ticket = binding
            .wfi_wait_ticket(current.vcpu_id)
            .unwrap_or_else(|error| {
                crate::diagnostics::crash::fatal(format_args!(
                    "interrupt gate ticket failed: {error:?}"
                ))
            });
        // Capture the ticket before both predicates so neither reopening nor
        // cancellation can be lost between this observation and parking.
        if super::runner::administrative_stop_reason(execution, thread).is_some()
            || !crate::hal::vm::interrupt_entry_gate_closed(current.interrupts())
        {
            return;
        }
        match binding.prepare_wfi_wait(current.vcpu_id, ticket) {
            Ok(crate::vm::endpoint::PreparedWait::Park(park)) => {
                let _ = park.complete();
            }
            Ok(_) => {}
            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                "interrupt gate park failed: {error:?}"
            )),
        }
    }
}
