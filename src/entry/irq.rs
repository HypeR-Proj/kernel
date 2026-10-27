// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Physical-interrupt entry policy.

use hyper::hal::interrupt::{EntryAction as Action, InterruptId, InterruptOrigin};

/// Services one acknowledged architecture-private kernel doorbell.
///
/// Architecture entry completes its hardware acknowledgement before crossing
/// this boundary. Mailbox polling runs first and releases all of its private
/// synchronization before deferred services may wake scheduler waiters.
pub(crate) fn dispatch_kernel_rpc() {
    crate::irq::cross_call::service();
    crate::time::service_irq_prompt();
    crate::task::scheduler::service_balance_irq_prompt();
    crate::reaper::service_irq_prompt();
    crate::diagnostics::log::service_irq_prompt();
    crate::fs::cache::worker::service_irq_prompt();
    crate::device::iommu::runtime::service_irq_prompt();
}

/// Services one architecture-private doorbell as a real outer IRQ entry.
///
/// The architecture acknowledges its private source before this call. Unlike
/// a registered hardware interrupt there is no domain interrupt number to
/// dispatch or complete, but preemption accounting and tail selection are
/// still required—RISC-V uses the same SSIP prompt for scheduler requests.
pub(crate) fn dispatch_kernel_rpc_entry(origin: InterruptOrigin) -> Action {
    let irq = match crate::task::preempt::enter_irq() {
        Ok(irq) => irq,
        Err(error) => crate::irq::exception::fatal_interrupt(error.description(), None),
    };
    crate::tracepoint!(Irq, "irq_enter", irq = 0, irq_kind = 1);
    dispatch_kernel_rpc();
    crate::tracepoint!(Irq, "irq_exit", irq = 0, irq_kind = 1);
    complete_entry(irq, Action::Resume { postlude: None }, origin, None)
}

/// Dispatches one architecture-acknowledged interrupt.
///
/// The caller owns interrupt acknowledgement and raw-frame lifetime. This
/// adapter completes the acknowledged source exactly once before returning
/// either action. Local interrupts must remain masked. Dispatch is
/// allocation-free but may invoke registered interrupt-safe handlers under the
/// IRQ registry lock.
pub(crate) fn dispatch(
    interrupt: InterruptId,
    origin: InterruptOrigin,
    frame: &crate::hal::irq::InterruptFrame<'_>,
) -> Action {
    let irq = match crate::task::preempt::enter_irq() {
        Ok(irq) => irq,
        Err(error) => crate::irq::exception::fatal_interrupt(error.description(), Some(interrupt)),
    };
    crate::tracepoint!(Irq, "irq_enter", irq = interrupt.get() as u64, irq_kind = 0);
    let action = if crate::diagnostics::crash::is_stop_interrupt(interrupt) {
        crate::irq::interrupt::complete(interrupt);
        Action::Stop
    } else {
        crate::irq::interrupt::dispatch(interrupt, frame);
        crate::time::service_irq_prompt();
        crate::task::scheduler::service_balance_irq_prompt();
        crate::reaper::service_irq_prompt();
        crate::diagnostics::log::service_irq_prompt();
        crate::fs::cache::worker::service_irq_prompt();
        crate::device::iommu::runtime::service_irq_prompt();
        Action::Resume { postlude: None }
    };
    crate::tracepoint!(Irq, "irq_exit", irq = interrupt.get() as u64, irq_kind = 0);
    complete_entry(irq, action, origin, Some(interrupt))
}

fn complete_entry(
    irq: crate::task::preempt::IrqGuard,
    action: Action,
    origin: InterruptOrigin,
    interrupt: Option<InterruptId>,
) -> Action {
    let native_unwind = origin.native_unwind();
    let irq_tail_postlude = native_unwind
        .is_none()
        .then_some(())
        .and_then(|()| guest_irq_postlude(origin));
    match irq.complete() {
        Ok(true)
            if matches!(action, Action::Resume { .. })
                && origin.is_guest()
                && irq_tail_postlude.is_some()
                && current_guest_stop_requested(interrupt) =>
        {
            Action::StopGuest {
                postlude: irq_tail_postlude,
            }
        }
        Ok(true)
            if matches!(action, Action::Resume { .. })
                && native_unwind.is_some()
                && match crate::task::preempt::should_reschedule_after_irq() {
                    Ok(pending) => pending,
                    Err(error) => {
                        crate::irq::exception::fatal_interrupt(error.description(), interrupt)
                    }
                } =>
        {
            Action::Resume {
                postlude: native_unwind,
            }
        }
        Ok(true)
            if matches!(action, Action::Resume { .. })
                && irq_tail_postlude.is_some()
                && guest_postlude_required(interrupt) =>
        {
            Action::Resume {
                postlude: irq_tail_postlude,
            }
        }
        Ok(_) => action,
        Err(_) => crate::irq::exception::fatal_interrupt(
            "failed to complete IRQ preemption accounting",
            interrupt,
        ),
    }
}

fn current_guest_stop_requested(interrupt: Option<InterruptId>) -> bool {
    match crate::vm::vcpu::current_administrative_stop_requested() {
        Ok(pending) => pending,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: IRQ-tail vCPU stop observation failed for IRQ {:?}: {error:?}",
            interrupt.map(InterruptId::get)
        )),
    }
}

fn guest_postlude_required(interrupt: Option<InterruptId>) -> bool {
    let reschedule = match crate::task::preempt::should_reschedule_after_irq() {
        Ok(pending) => pending,
        Err(error) => crate::irq::exception::fatal_interrupt(error.description(), interrupt),
    };
    if reschedule {
        return true;
    }
    match crate::vm::vcpu::current_interrupt_reconcile_pending() {
        Ok(pending) => pending,
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: IRQ-tail vCPU reconcile observation failed: {error:?}"
        )),
    }
}

/// Runs the outermost guest interrupt-exit reconciliation and scheduling point.
///
/// Local interrupts must remain masked and architecture entry must have
/// completed interrupt acknowledgement before calling this adapter. No raw
/// exception-frame reference crosses this boundary. If the interrupted Thread
/// owns an active vCPU, its local virtual hardware is unpublished and saved
/// before optional scheduling and then restored by the same suspended
/// continuation. Interrupt-only work therefore refills virtual hardware
/// without manufacturing scheduler policy.
fn guest_irq_tail(tail: crate::hal::exception::IrqTailCapability) {
    let current_vcpu = match crate::task::scheduler::current_vcpu_if_present() {
        Ok(current) => current,
        Err(error) => fail_guest_irq_tail("failed to identify interrupted Thread", error),
    };

    if let Some(current) = current_vcpu {
        let execution = typed_vcpu_execution(current);
        // SAFETY: The scheduler supplied its pinned current-vCPU owner pointer.
        // IRQ dispatch has returned, so no active-vCPU callback borrow remains,
        // and local interrupts stay masked across unpublication and save.
        if let Err(error) = unsafe { crate::vm::vcpu::deactivate(&mut *execution) } {
            fail_vcpu_tail("failed to deactivate interrupted vCPU", error)
        }
    }

    if let Err(error) = crate::task::scheduler::cond_resched_from_irq_tail(tail) {
        fail_guest_irq_tail("IRQ-tail scheduling failed", error)
    }

    if let Some(interrupted) = current_vcpu {
        // This continuation may have been Ready or Migrating while another
        // CPU published administrative stop. Reobserve the exact now-current
        // vCPU after scheduling and before any hardware reactivation.
        let resumed = match crate::task::scheduler::current_vcpu_if_present() {
            Ok(Some(current)) => current,
            Ok(None) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: detached vCPU IRQ-tail resumed as a non-vCPU Thread"
            )),
            Err(error) => fail_guest_irq_tail("failed to reobserve resumed vCPU", error),
        };
        if resumed.thread != interrupted.thread || resumed.execution != interrupted.execution {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: detached vCPU IRQ-tail resumed with different scheduler ownership"
            ));
        }
        let execution = typed_vcpu_execution(resumed);
        loop {
            // SAFETY: ordinary IRQ-tail deactivation completed before scheduling,
            // the query above returned the exact current continuation, and local
            // interrupts remain masked.
            let stopped =
                match unsafe { crate::vm::vcpu::complete_detached_stop_if_requested(resumed) } {
                    Ok(stopped) => stopped,
                    Err(error) => fail_detached_stop(error),
                };
            if stopped {
                // Nonreturning scheduler exit lets the suspended continuation
                // unwind through its normal Thread destruction/reap path without
                // ever restoring guest hardware.
                crate::task::scheduler::exit_current()
            }
            // SAFETY: This continuation can resume only when its pinned vCPU Thread
            // is current again. The preceding deactivation removed all local
            // architectural ownership, and interrupts are still masked.
            if let Err(error) = unsafe { crate::vm::vcpu::activate(execution) } {
                if error == crate::vm::vcpu::HardwareTransitionError::InterruptGateClosed {
                    // SAFETY: Failed admission retains no live hardware or claim.
                    // The suspended Thread owns this exact pinned execution.
                    unsafe {
                        crate::vm::vcpu::wait_for_interrupt_gate(execution, resumed.thread);
                    }
                    continue;
                }
                if error
                    == crate::vm::vcpu::HardwareTransitionError::Execution(
                        crate::vm::registry::VmExecutionError::AdmissionClosed,
                    )
                {
                    // Admission is closed only after every bound endpoint has an
                    // exact durable stop request. Recheck that authority instead
                    // of treating an unrelated close as a graceful guest exit.
                    // SAFETY: activation failed before publication and hardware
                    // ownership, while the exact current vCPU remains pinned.
                    let stopped = match unsafe {
                        crate::vm::vcpu::complete_detached_stop_if_requested(resumed)
                    } {
                        Ok(stopped) => stopped,
                        Err(error) => fail_detached_stop(error),
                    };
                    if stopped {
                        crate::task::scheduler::exit_current()
                    }
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: IRQ-tail VM admission closed without an exact vCPU stop request"
                    ));
                }
                fail_vcpu_tail("failed to reactivate interrupted vCPU", error)
            }
            break;
        }
    }
}

fn typed_vcpu_execution(
    current: crate::task::scheduler::CurrentVcpu,
) -> *mut crate::vm::vcpu::VcpuExecution {
    match current
        .execution
        .downcast::<crate::vm::vcpu::VcpuExecution>()
    {
        Some(execution) => execution.as_ptr(),
        None => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: scheduler returned a vCPU with the wrong execution payload type"
        )),
    }
}

/// Runs the kernel-selected interrupt postlude.
///
/// # Safety
///
/// The caller must satisfy the postlude contract documented by
/// [`Action::Resume`].
unsafe extern "C" fn selected_guest_irq_postlude() {
    // SAFETY: selected entry publishes this callback only when it can invoke
    // it after interrupt completion, on the interrupted Thread stack, with
    // local IRQs masked.
    unsafe { crate::hal::exception::with_irq_tail_capability(guest_irq_tail) };
}

#[inline]
fn guest_irq_postlude(origin: InterruptOrigin) -> Option<unsafe extern "C" fn()> {
    crate::hal::exception::qualify_irq_tail_postlude(origin, selected_guest_irq_postlude)
}

fn fail_guest_irq_tail(operation: &str, error: crate::task::scheduler::Error) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {operation}: {error:?}"))
}

fn fail_vcpu_tail(operation: &str, error: crate::vm::vcpu::HardwareTransitionError) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {operation}: {error:?}"))
}

fn fail_detached_stop(error: crate::vm::vcpu::DetachedStopError) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: detached IRQ-tail vCPU stop completion failed: {error:?}"
    ))
}

/// Claims and dispatches one external controller interrupt, when pending.
///
/// This is the RISC-V external-interrupt entry seam. Controller claim and
/// registry dispatch are kernel-owned policy; architecture code only decodes
/// the architectural trap cause.
// Only the RISC-V trap backend uses a controller claim distinct from its
// architectural interrupt acknowledgement. Keeping target selection below
// `arch` leaves this narrow adapter intentionally unused on other targets.
#[allow(dead_code)]
pub(crate) fn claim_and_dispatch_external(
    origin: InterruptOrigin,
    frame: &crate::hal::irq::InterruptFrame<'_>,
) -> Option<Action> {
    crate::irq::acknowledge_external().map(|interrupt| dispatch(interrupt, origin, frame))
}

/// Publishes a remote CPU's exact interrupt snapshot and stops that CPU.
pub(crate) fn stop(context: crate::hal::exception::CrashContext) -> ! {
    crate::diagnostics::crash::stop_this_cpu(context)
}
