// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest power-call dispatch and stopped scheduler continuation.

use super::VcpuExecution;
use hyper_vm_models::arm::psci::Continuation;

/// Checks for a new bootstrap on first scheduling of a secondary vCPU.
/// The caller owns detached hardware and has not entered guest execution.
pub(super) fn first_entry(execution: &mut VcpuExecution) {
    let Some(binding) = execution.vm_binding() else {
        return;
    };
    if let Continuation::Restart(bootstrap) =
        binding.lifecycle().power_continuation(execution.vcpu_id)
    {
        reset(execution, bootstrap);
    }
}

fn reset(execution: &mut VcpuExecution, bootstrap: hyper_vm_models::arm::psci::Bootstrap) {
    if let Err(error) = crate::hal::vm::reset_power_context(
        &mut execution.hardware,
        bootstrap.entry,
        bootstrap.context,
    ) {
        crate::diagnostics::crash::fatal(format_args!("vCPU power-on context failed: {error:?}"));
    }
}

/// Parks a stopped vCPU without a timer. Endpoint generation arbitration
/// covers completion-before-park, interrupt prompts, and administrative stop.
/// Returns false only after finishing the administrative detach lifecycle.
pub(super) unsafe fn wait(
    execution: *mut VcpuExecution,
    thread: crate::task::thread::ThreadId,
) -> bool {
    loop {
        if let Some(reason) = super::runner::administrative_stop_reason(execution, thread) {
            super::runner::finish_inactive_administrative_stop(execution, thread, reason);
            return false;
        }
        // SAFETY: The scheduler exclusively owns this pinned, detached payload.
        let current = unsafe { &mut *execution };
        let Some(binding) = current.vm_binding() else {
            hyper::debug::invariant_failure("vm::vcpu::power::wait invariant");
        };
        let ticket = binding
            .wfi_wait_ticket(current.vcpu_id)
            .unwrap_or_else(|_| hyper::debug::invariant_failure("vm::vcpu::power::wait invariant"));
        match binding.lifecycle().power_continuation(current.vcpu_id) {
            Continuation::Resume(value) => {
                if let Err(error) =
                    crate::hal::vm::complete_power_call(&mut current.hardware, value)
                {
                    crate::diagnostics::crash::fatal(format_args!(
                        "vCPU power completion failed: {error:?}"
                    ));
                }
                return true;
            }
            Continuation::Restart(bootstrap) => {
                reset(current, bootstrap);
                return true;
            }
            Continuation::Wait => {}
        }
        match binding.prepare_wfi_wait(current.vcpu_id, ticket) {
            Ok(crate::vm::endpoint::PreparedWait::Park(park)) => {
                let _ = park.complete();
            }
            Ok(_) => {}
            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                "vCPU power request park failed: {error:?}"
            )),
        }
    }
}
