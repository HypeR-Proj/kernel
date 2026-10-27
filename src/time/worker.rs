// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-CPU normal-context delivery of expired callouts.
//!
//! Expiry publishes durable work without entering scheduler synchronization.
//! IRQ-tail service converts that request into a counted wake after interrupt
//! dispatch has released its registry locks. Each pinned worker executes a
//! bounded batch without holding the deadline or ready-queue locks.

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::{DeferredWork, PublishedOnce, WorkDisposition};

use crate::sync::Completion;
use crate::task::scheduler;
use crate::task::thread::ThreadId;

const WORK_ITEMS_PER_BATCH: usize = 32;

struct Worker {
    work: DeferredWork,
    wake: Completion,
    thread: PublishedOnce<ThreadId>,
}

impl Worker {
    const fn new() -> Self {
        Self {
            work: DeferredWork::new(),
            wake: Completion::new(),
            thread: PublishedOnce::new(),
        }
    }
}

static WORKERS: PerCpu<Worker> = PerCpu::new([const { Worker::new() }; hyper::cpu::MAX_CPUS]);

/// Starts one critical FIFO worker per admitted CPU after timer initialization.
///
/// Workers retain their source CPU affinity for their entire lifetime. CPU
/// hotplug must introduce an explicit queue-drain phase before removing one.
pub(crate) fn initialize() -> Result<(), scheduler::Error> {
    let count = crate::cpu::participating_cpu_count().ok_or(scheduler::Error::NotInitialized)?;
    for index in 0..count {
        let cpu = CpuIndex::new(index).ok_or(scheduler::Error::InvalidCpuIndex)?;
        let worker = &WORKERS[cpu];
        if worker.thread.get().is_some() {
            return Err(scheduler::Error::AlreadyInitialized);
        }
        let thread = scheduler::kthread_create_per_cpu("ktimer", cpu, worker_entry, cpu.get())?;
        scheduler::set_thread_fifo_policy(thread, crate::task::policy::ThreadPriority::new(32))?;
        if !worker.work.claim_initial_worker() || worker.thread.publish(thread).is_err() {
            return Err(scheduler::Error::AlreadyInitialized);
        }
        // The initial worker owns work published before it became runnable.
        // An old, unissued prompt must not suppress its first later wakeup.
        let _ = worker.work.consume_prompt();
        if !scheduler::thread_ready(thread)? {
            return Err(scheduler::Error::InvalidThreadState);
        }
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    super::tick::validation::expect_cpus(count);
    Ok(())
}

pub(super) fn is_ready(cpu: CpuIndex) -> bool {
    WORKERS[cpu].thread.get().is_some()
}

/// Publishes a ready-queue change without allocating or taking scheduler locks.
/// The producer must link its owned delivery before calling this function.
pub(super) fn request(cpu: CpuIndex) {
    if WORKERS[cpu].work.request() && is_ready(cpu) {
        crate::irq::reschedule::notify(cpu);
    }
}

/// Identifies the actual Thread, including when its callback was preempted.
/// A CPU-local callback-active flag would misclassify unrelated continuations.
pub(super) fn is_current_worker() -> Result<bool, scheduler::Error> {
    let current = scheduler::current_thread_id()?;
    Ok(WORKERS
        .iter()
        .any(|worker| worker.thread.get().is_some_and(|thread| *thread == current)))
}

/// Services comparator and worker prompts outside the interrupt registry lock.
pub(crate) fn service_irq_prompt() {
    if let Err(error) = super::timers::service_scheduler_deadline() {
        crate::irq::exception::fatal_timer(error);
    }
    if let Err(error) = super::timers::service_fixed_deadlines() {
        crate::irq::exception::fatal_timer(error);
    }
    if let Err(error) = super::timers::service_reprogram_prompt() {
        crate::irq::exception::fatal_timer(error);
    }
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    let worker = &WORKERS[cpu];
    if worker.thread.get().is_none()
        || !worker.work.consume_prompt()
        || !worker.work.claim_notification()
    {
        return;
    }
    if let Err(error) = worker.wake.complete() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: timer worker wake failed: {error:?}"
        ));
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn permanent_worker_count_for_test() -> usize {
    WORKERS
        .iter()
        .filter(|worker| worker.thread.get().is_some())
        .count()
}

extern "C" fn worker_entry(argument: usize) {
    let Some(cpu) = CpuIndex::new(argument) else {
        hyper::debug::invariant_failure("timer worker CPU is invalid");
    };
    if crate::cpu::current_index() != Some(cpu) {
        hyper::debug::invariant_failure("timer worker escaped its CPU affinity");
    }
    let worker = &WORKERS[cpu];
    loop {
        worker.work.begin_batch();
        for _ in 0..WORK_ITEMS_PER_BATCH {
            if !super::callout::run_one(cpu) {
                break;
            }
        }
        match worker.work.finish_batch(super::callout::has_ready(cpu)) {
            WorkDisposition::Continue => {
                if let Err(error) = scheduler::cond_resched() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: timer worker reschedule failed: {error:?}"
                    ));
                }
            }
            WorkDisposition::Wait => {
                if let Err(error) = worker.wake.wait() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: timer worker wait failed: {error:?}"
                    ));
                }
            }
        }
    }
}
