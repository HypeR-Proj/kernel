// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler reschedule cross-call policy.
//!
//! The scheduler's per-CPU pending bit is the durable condition. This module
//! installs the architecture interrupt which prompts a target CPU to evaluate
//! that condition at IRQ-tail; the handler itself owns no scheduler state.

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::{InterruptPriority, InterruptTrigger};
use hyper::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper::{cpu::PerCpu, sync::atomic::AtomicUsize};

use super::interrupt::{HandlerResult, IrqDomainId, VirtualInterrupt};

static READY: AtomicBool = AtomicBool::new(false);
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static DELIVERY_COUNT: PerCpu<AtomicUsize> =
    PerCpu::new([const { AtomicUsize::new(0) }; hyper::cpu::MAX_CPUS]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    CpuAdmissionStarted,
    Interrupt(super::interrupt::Error),
    InterruptsEnabled,
    SchedulerUnavailable,
}

/// Installs the permanent per-CPU reschedule interrupt before SMP admission.
pub(crate) fn initialize(root_domain: IrqDomainId) -> Result<(), Error> {
    // This single-CPU, IRQ-masked phase makes the bootstrap-pending snapshot
    // and READY publication one transaction with respect to all schedulable
    // execution. Runtime callers observe READY only after handler publication.
    if crate::hal::irq::local_enabled() {
        return Err(Error::InterruptsEnabled);
    }
    if crate::cpu::online_cpu_count() != 0 {
        return Err(Error::CpuAdmissionStarted);
    }
    // The scheduler precedes IRQ setup in the boot contract. Observe any
    // request published during that narrow early window before enabling the
    // cross-call, so its elected notification cannot remain only an event.
    let bootstrap_pending =
        crate::task::preempt::pending(CpuIndex::BOOT).map_err(|_| Error::SchedulerUnavailable)?;
    let Some(hardware_interrupt) = crate::hal::irq::reschedule_interrupt() else {
        READY.store(true, Ordering::Release);
        if bootstrap_pending {
            notify(CpuIndex::BOOT);
        }
        crate::pr_notice!("HypeR: reschedule prompts use architecture-private wake delivery");
        return Ok(());
    };
    let (_, registration) = root_domain
        .register_shared_mapping(
            hardware_interrupt,
            InterruptPriority::High,
            InterruptTrigger::Edge,
            0,
            handle,
        )
        .map_err(Error::Interrupt)?;
    // Scheduler cross-calls remain installed for every CPU's online lifetime.
    // HypeR has no CPU-hotplug teardown phase in which removal would be safe.
    registration.retain_permanently();
    READY.store(true, Ordering::Release);
    if bootstrap_pending {
        notify(CpuIndex::BOOT);
    }
    crate::pr_info!(
        "HypeR: targeted reschedule cross-call initialized on INTID {}",
        hardware_interrupt.get()
    );
    Ok(())
}

/// Prompts `cpu` to enter the deferred IRQ service and scheduling boundary.
///
/// Scheduler callers publish their pending request before this call. Other
/// deferred services publish an independent durable condition which IRQ entry
/// translates before scheduling. Architecture routing is validated before a
/// CPU becomes scheduler-visible. The event fallback can wake an idle CPU;
/// backends without a targeted interrupt remain cooperative for a busy CPU
/// and cannot claim bounded remote preemption or deadline reprogramming.
/// There is no periodic scheduler tick to provide an implicit retry.
pub(crate) fn notify(cpu: CpuIndex) {
    if !READY.load(Ordering::Acquire) || !crate::hal::irq::notify_reschedule(cpu) {
        crate::hal::cpu::send_event();
    }
}

fn handle(
    _interrupt: VirtualInterrupt,
    _context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    if let Some(cpu) = crate::cpu::current_index() {
        // The counter is only an observation point; it publishes no protected
        // state and therefore requires no ordering beyond atomic coherence.
        DELIVERY_COUNT[cpu].fetch_add(1, Ordering::Relaxed);
    }
    // IRQ-tail consumes the already-published scheduler request after the
    // generic interrupt entry completes accounting and controller EOI.
    HandlerResult::Handled
}

/// Returns the number of reschedule interrupts dispatched on `cpu`.
///
/// This observation seam is compiled only into the bare-metal self-test image.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[allow(
    dead_code,
    reason = "IPI delivery counters are exercised by AArch64 and x86 runtime tests"
)]
pub(super) fn delivery_count_for_test(cpu: CpuIndex) -> usize {
    DELIVERY_COUNT[cpu].load(Ordering::Relaxed)
}
