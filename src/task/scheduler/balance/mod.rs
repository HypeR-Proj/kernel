// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded placement repair at scheduling events; no periodic maintenance wake.

mod retry;
mod work;

use super::{
    SCHEDULER, current_cpu, current_thread_id, publish_committed_ready, scheduler_invariant,
};
use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);
static WORK: work::Work = work::Work::new();
static RETRY: retry::ReleaseRetry = retry::ReleaseRetry::new();
static PROMPTS: PerCpu<AtomicBool> =
    PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);

pub(crate) fn initialize() -> Result<(), crate::time::Error> {
    ENABLED.store(true, Ordering::Release);
    request();
    if let Ok(cpu) = current_cpu() {
        prompt(cpu);
    }
    Ok(())
}

fn work_pending() -> bool {
    WORK.pending()
}

/// An active pass always retains CONTINUE until its atomic completion, so this
/// single observation also excludes an owner between scan and publication.
#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
pub(crate) fn idle_for_test() -> bool {
    !WORK.pending()
}

fn prompt(cpu: CpuIndex) {
    if !PROMPTS[cpu].swap(true, Ordering::AcqRel) {
        crate::irq::reschedule::notify(cpu);
    }
}

pub(super) fn request() {
    if WORK.request()
        && ENABLED.load(Ordering::Acquire)
        && let Ok(cpu) = current_cpu()
    {
        prompt(cpu);
    }
}

/// The lock invokes this only for a failed try's subscribed lane, after release.
/// No scheduler state is accessed from a reader/writer's teardown path.
pub(super) fn coordinator_released() {
    if let Some(cpu) = RETRY.released().and_then(CpuIndex::new) {
        prompt(cpu);
    }
}

/// Deferred IRQ service preserves a continuation past the scheduling decision
/// that queued its IPI. It does not take scheduler locks or scan the registry.
pub(crate) fn service_irq_prompt() {
    let Ok(cpu) = current_cpu() else { return };
    if !PROMPTS[cpu].load(Ordering::Relaxed) {
        return;
    }
    if PROMPTS[cpu].swap(false, Ordering::AcqRel)
        && ENABLED.load(Ordering::Acquire)
        && work_pending()
        && let Err(error) =
            crate::task::preempt::request(cpu, crate::task::preempt::RescheduleSource::Balance)
    {
        scheduler_invariant("load balance prompt", error.into());
    }
}

/// One serialized pass visits at most 64 slots and moves at most four Threads.
/// Contention subscribes to the actual blocking lane's next release; it never
/// retries with an immediate self-IPI while a registry reader is still active.
pub(super) fn service() {
    if !ENABLED.load(Ordering::Acquire) || !work_pending() {
        return;
    }
    // SAFETY: no scheduling occurs inside the bounded pass. Pinning the owner
    // also prevents its release notification from running before ACTIVE clears.
    let _mask = unsafe { super::TransitionMask::acquire() };
    let Ok(cpu) = current_cpu() else { return };
    let Ok(current) = current_thread_id() else {
        return;
    };
    if !WORK.begin() {
        return;
    }
    // New events stay dirty while an existing sweep advances to its captured
    // high-water mark. They request a later sweep rather than resetting it.
    RETRY.register(cpu.get());
    let result = SCHEDULER.try_with(|slot| {
        RETRY.acquired();
        slot.as_mut()
            .ok_or(super::Error::NotInitialized)?
            .balance_ready(current)
    });
    if let Some(result) = result {
        let more = match result {
            Ok(pass) => {
                for outcome in pass.outcomes.into_iter().flatten() {
                    publish_committed_ready(outcome);
                }
                pass.more
            }
            Err(error) => scheduler_invariant("event load balancing", error),
        };
        if WORK.complete(more) {
            prompt(cpu);
        }
    } else {
        // The subscribed release will prompt this still-pinned CPU. In-flight
        // callbacks may have already set PROMPTS, but its IRQ cannot run yet.
        WORK.blocked();
    }
}
