// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-owned continuation of a userspace-emulated MMIO instruction.

use super::VcpuExecution;

/// The raw payload is scheduler-pinned and hardware-detached for this entire
/// continuation, just as for WFI and PSCI waits. No raw-frame borrow survives.
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
            hyper::debug::invariant_failure("vm::vcpu::mmio::wait invariant");
        };
        let ticket = binding
            .wfi_wait_ticket(current.vcpu_id)
            .unwrap_or_else(|_| hyper::debug::invariant_failure("vm::vcpu::mmio::wait invariant"));
        let lifecycle = binding.lifecycle();
        match lifecycle.take_mmio_completion(current.vcpu_id) {
            Ok(Some(hyper_vm_models::exit::MmioAction::Stop)) => {
                crate::vm::installed::InstalledMachine::request_stop(&lifecycle);
            }
            Ok(Some(action)) => {
                if let Err(error) =
                    crate::hal::vm::complete_device_call(&mut current.hardware, action)
                {
                    crate::diagnostics::crash::fatal(format_args!(
                        "invalid detached MMIO completion: {error:?}"
                    ));
                }
                return true;
            }
            Ok(None) => {}
            Err(_) => {
                // Administrative stop may have cancelled the slot after the
                // loop's first observation; the published endpoint stop wins.
                if super::runner::administrative_stop_reason(execution, thread).is_some() {
                    continue;
                }
                crate::diagnostics::crash::fatal(format_args!(
                    "MMIO continuation lost its pending request"
                ));
            }
        }
        match binding.prepare_wfi_wait(current.vcpu_id, ticket) {
            Ok(crate::vm::endpoint::PreparedWait::Park(park)) => {
                let _ = park.complete();
            }
            Ok(_) => {}
            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                "MMIO continuation park failed: {error:?}"
            )),
        }
    }
}

/// An internal controller transaction waits with all of this vCPU's hardware
/// detached. In particular it cannot prevent an oversubscribed peer from
/// running its save path on this same host CPU.
pub(super) unsafe fn wait_interrupt_access(
    execution: *mut VcpuExecution,
    thread: crate::task::thread::ThreadId,
) -> bool {
    loop {
        // SAFETY: This continuation owns the scheduler-pinned detached payload.
        let current = unsafe { &*execution };
        let Some(binding) = current.vm_binding() else {
            hyper::debug::invariant_failure("vGIC continuation missing binding");
        };
        let ticket = binding
            .wfi_wait_ticket(current.vcpu_id)
            .unwrap_or_else(|_| hyper::debug::invariant_failure("vGIC wait ticket"));
        if let Some(reason) = super::runner::administrative_stop_reason(execution, thread) {
            crate::hal::vm::cancel_interrupt_access(binding.interrupts(), current.vcpu_id);
            binding.publish_changed_interrupts();
            super::runner::finish_inactive_administrative_stop(execution, thread, reason);
            return false;
        }
        if let Some(action) =
            crate::hal::vm::take_interrupt_access(binding.interrupts(), current.vcpu_id)
        {
            if action == hyper_vm_models::exit::MmioAction::Stop {
                crate::vm::installed::InstalledMachine::request_stop(&binding.lifecycle());
                continue;
            }
            // SAFETY: No binding borrow survives into the exclusive saved-frame update.
            let current = unsafe { &mut *execution };
            if let Err(error) = crate::hal::vm::complete_device_call(&mut current.hardware, action)
            {
                crate::diagnostics::crash::fatal(format_args!(
                    "invalid GIC device completion: {error:?}"
                ));
            }
            return true;
        }
        match binding.prepare_wfi_wait(current.vcpu_id, ticket) {
            Ok(crate::vm::endpoint::PreparedWait::Park(park)) => {
                let _ = park.complete();
            }
            Ok(_) => {}
            Err(error) => {
                crate::diagnostics::crash::fatal(format_args!("GIC MMIO park failed: {error:?}"))
            }
        }
    }
}
