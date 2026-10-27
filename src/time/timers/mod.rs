// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-CPU software timers multiplexed onto one architectural comparator.

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, Ordering};
use {
    crate::time::queue::OwnedDeadlineQueue, crate::time::queue::PendingTimer,
    crate::time::queue::TimerQueueError,
};

use super::deadline_slot::{DeadlineMailbox, DeadlineSlot, earlier};
use crate::task::scheduler;

mod irq_callout;
pub(crate) use irq_callout::{ArmedIrqCallout, IrqCallout, OwnedArmedIrqCallout};

pub use {
    crate::time::queue::QueueStats, crate::time::queue::TimerCallback,
    crate::time::queue::TimerEvent, crate::time::queue::TimerHandle, crate::time::queue::TimerMode,
};

/// Bound one interrupt's callback work without imposing a timer storage cap.
const MAX_CALLBACKS_PER_INTERRUPT: usize = 64;

type TimerLock = InterruptSpinLock<ProcessorTimers, crate::hal::irq::LocalMask>;

static TIMERS: PerCpu<TimerLock> =
    PerCpu::new([const { TimerLock::new(ProcessorTimers::new()) }; hyper::cpu::MAX_CPUS]);

static REPROGRAM: PerCpu<AtomicBool> =
    PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);
static FIXED_READY: PerCpu<AtomicBool> =
    PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);
static SCHEDULER_MAILBOX: PerCpu<DeadlineMailbox> =
    PerCpu::new([const { DeadlineMailbox::new() }; hyper::cpu::MAX_CPUS]);

struct ProcessorTimers {
    initialized: bool,
    queue: OwnedDeadlineQueue,
    ready: super::callout::ReadyQueue,
    scheduler: DeadlineSlot,
    guest_recovery: DeadlineSlot,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    health_probe: DeadlineSlot,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    scheduler_expiries: u64,
}

impl ProcessorTimers {
    const fn new() -> Self {
        Self {
            initialized: false,
            queue: OwnedDeadlineQueue::new(),
            ready: super::callout::ReadyQueue::new(),
            scheduler: DeadlineSlot::new(),
            guest_recovery: DeadlineSlot::new(),
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            health_probe: DeadlineSlot::new(),
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            scheduler_expiries: 0,
        }
    }

    fn next_deadline(&self) -> Option<u64> {
        let deadline = earlier(
            self.queue.next_programming_deadline(),
            earlier(self.scheduler.deadline(), self.guest_recovery.deadline()),
        );
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        let deadline = earlier(deadline, self.health_probe.deadline());
        deadline
    }
}

/// Publishes a scheduling decision while holding the target CPU scheduler lock.
///
/// `generation` must increase for every change, including cancellation. A
/// publisher never takes the timer lock: callout waits already nest that lock
/// outside scheduler state. IRQ-tail or the final resume boundary consumes the
/// mailbox after releasing scheduler locks. Publication before local timer
/// initialization is retained and installed during initialization.
/// Every first pending publication prompts its target, including the local
/// CPU: a kernel Thread can continue running without a nearby resume boundary.
/// Deadlines use the architectural counter and must be within half a counter
/// period of the current time, like ordinary software timers.
pub(crate) fn publish_scheduler_deadline(
    cpu: CpuIndex,
    generation: u64,
    deadline: Option<u64>,
) -> Result<(), super::Error> {
    if SCHEDULER_MAILBOX[cpu].publish(generation, deadline) {
        crate::irq::reschedule::notify(cpu);
    }
    Ok(())
}

/// Installs a pending policy decision before the local CPU resumes execution.
///
/// The caller must have released scheduler and IRQ registry locks. Pinning
/// keeps the selected mailbox and architectural comparator on the same CPU.
pub(crate) fn service_scheduler_deadline() -> Result<(), super::Error> {
    let (cpu_pin, cpu) = pin_current_timer_cpu()?;
    let result = if let Some((generation, deadline)) = SCHEDULER_MAILBOX[cpu].take() {
        TIMERS[cpu].with(|timers| {
            if timers.scheduler.publish(generation, deadline) && timers.initialized {
                program_next(timers)?;
            }
            Ok(())
        })
    } else {
        Ok(())
    };
    release_timer_cpu_pin(cpu_pin);
    result
}

/// Observes the installed slot without publishing work, charging runtime, or
/// sending an IPI. Use outside scheduler locks, like other timer operations.
#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulerDeadlineObservation {
    pub(crate) generation: u64,
    pub(crate) deadline: Option<u64>,
    pub(crate) expiries: u64,
}

#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
pub(crate) fn scheduler_deadline_for_test(cpu: CpuIndex) -> SchedulerDeadlineObservation {
    TIMERS[cpu].with(|timers| SchedulerDeadlineObservation {
        generation: timers.scheduler.generation(),
        deadline: timers.scheduler.deadline(),
        expiries: timers.scheduler_expiries,
    })
}

/// Updates the active local guest timer's exceptional deassertion retry.
/// This slot is never used as an unconditional guest or host polling tick.
pub(crate) fn publish_guest_timer_recovery(
    generation: u64,
    deadline: Option<u64>,
) -> Result<(), super::Error> {
    let (cpu_pin, cpu) = pin_current_timer_cpu()?;
    let result = TIMERS[cpu].with(|timers| {
        let before = timers.next_deadline();
        if !timers.guest_recovery.publish(generation, deadline)
            || !timers.initialized
            || timers.next_deadline() == before
        {
            return Ok(());
        }
        program_next(timers)
    });
    release_timer_cpu_pin(cpu_pin);
    result
}

/// Allocation-free startup probes are absent from ordinary builds and stop
/// after their bounded validation sequence; they are not a replacement tick.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) fn publish_health_probe(generation: u64, deadline: u64) -> Result<(), super::Error> {
    let cpu = current_cpu()?;
    TIMERS[cpu].with(|timers| {
        if timers.health_probe.publish(generation, Some(deadline)) {
            program_next(timers)?;
        }
        Ok(())
    })
}

pub(super) fn initialize_local() -> Result<(), super::Error> {
    let cpu = current_cpu()?;
    TIMERS[cpu].with(|timers| {
        if timers.initialized {
            return Err(super::Error::TimerQueueAlreadyInitialized);
        }
        crate::hal::time::disable_local_timer();
        timers.queue.initialize_id(cpu.get())?;
        // A scheduler decision may precede comparator setup, including on a
        // secondary CPU before it can receive cross-calls. Consume its durable
        // mailbox while local IRQs are masked; no scheduler lock is acquired.
        if let Some((generation, deadline)) = SCHEDULER_MAILBOX[cpu].take() {
            timers.scheduler.publish(generation, deadline);
        }
        timers.initialized = true;
        program_next(timers)
    })
}

pub fn schedule_irq_at(
    deadline: u64,
    mode: TimerMode,
    callback: TimerCallback,
    context: usize,
) -> Result<TimerHandle, super::Error> {
    let pending = PendingTimer::try_new(deadline, mode, callback, context)?;
    // Allocation must precede pinning: allocator slow paths may reach a
    // scheduling point, while the selected queue and comparator are CPU-local.
    let (cpu_pin, cpu) = pin_current_timer_cpu()?;
    if let Err(error) = TIMERS[cpu].with(|timers| ensure_initialized(timers)) {
        release_timer_cpu_pin(cpu_pin);
        return Err(error);
    }
    let (result, retired, rollback_failed) = TIMERS[cpu].with(|timers| {
        let previous = timers.queue.next_deadline();
        let handle = timers.queue.insert(pending, super::monotonic_ticks());
        if timers.queue.next_deadline() != previous
            && let Err(error) = program_next(timers)
        {
            let retired = match timers.queue.cancel(handle) {
                Ok(retired) => Some(retired),
                // The exact handle was inserted while this lock was held. If
                // rollback cannot find it, returning would abandon a possibly
                // live callback context. Report the invariant only after the
                // timer lock and local interrupt mask have been released.
                Err(_) => return (Err(error), None, true),
            };
            return (Err(error), retired, false);
        }
        (Ok(handle), None, false)
    });
    release_timer_cpu_pin(cpu_pin);
    // Queue retirement may enter the allocator and therefore happens only
    // after the CPU-local comparator transaction is complete and unpinned.
    drop(retired);
    if rollback_failed {
        // The caller that supplied `context` cannot observe a return, so its
        // callback owner remains live while coordinated crash handling stops
        // every CPU that could consume the corrupt timer queue.
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: exact timer insertion rollback failed"
        ));
    }
    result
}

pub fn schedule_irq_after(
    nanoseconds: u64,
    callback: TimerCallback,
    context: usize,
) -> Result<TimerHandle, super::Error> {
    let deadline = super::deadline_after(nanoseconds)?;
    schedule_irq_at(deadline, TimerMode::OneShot, callback, context)
}

pub fn schedule_irq_periodic(
    first_deadline: u64,
    interval_ticks: u64,
    callback: TimerCallback,
    context: usize,
) -> Result<TimerHandle, super::Error> {
    schedule_irq_at(
        first_deadline,
        TimerMode::Periodic {
            interval: interval_ticks,
        },
        callback,
        context,
    )
}

/// Cancels a timer on the queue identified by its handle.
///
/// A Thread may resume on a different CPU from the one on which it armed a
/// timer. Cancellation therefore follows the handle back to its source queue.
/// Only the source CPU can update its architectural comparator: remote
/// cancellation may leave an obsolete earlier deadline programmed, causing at
/// most one harmless timer interrupt which then programs the next deadline.
pub fn cancel_irq_timer(handle: TimerHandle) -> Result<(), super::Error> {
    let owner = CpuIndex::new(handle.queue_id()).ok_or(super::Error::InvalidCpuIndex)?;
    let (cpu_pin, current) = pin_current_timer_cpu()?;
    let cancelled = TIMERS[owner].with(|timers| {
        ensure_initialized(timers)?;
        let previous = timers.queue.next_deadline();
        let retired = timers.queue.cancel(handle)?;
        let result = if owner == current && timers.queue.next_deadline() != previous {
            program_next(timers)
        } else {
            Ok(())
        };
        Ok::<_, super::Error>((result, retired))
    });
    release_timer_cpu_pin(cpu_pin);
    let (result, retired) = cancelled?;
    drop(retired);
    result
}

pub fn local_statistics() -> Option<QueueStats> {
    let (cpu_pin, cpu) = pin_current_timer_cpu().ok()?;
    let statistics = TIMERS[cpu].with(|timers| timers.initialized.then(|| timers.queue.stats()));
    release_timer_cpu_pin(cpu_pin);
    statistics
}

pub(super) fn handle_interrupt() -> Result<usize, super::Error> {
    let cpu = current_cpu()?;
    crate::hal::time::mask_local_timer();
    let mut callbacks = 0;
    while callbacks < MAX_CALLBACKS_PER_INTERRUPT {
        let expired = TIMERS[cpu].with(|timers| {
            ensure_initialized(timers)?;
            Ok::<_, super::Error>(timers.queue.pop_expired(super::monotonic_ticks()))
        })?;
        let Some(expired) = expired else {
            break;
        };
        expired.invoke();
        callbacks += 1;
    }
    TIMERS[cpu].with(|timers| {
        ensure_initialized(timers)?;
        let now = super::monotonic_ticks();
        let scheduler_ready = timers.scheduler.expire(now);
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        if scheduler_ready {
            timers.scheduler_expiries = timers.scheduler_expiries.saturating_add(1);
        }
        let recovery_ready = timers.guest_recovery.expire(now);
        let ready = scheduler_ready || recovery_ready;
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        let ready = timers.health_probe.expire(now) || ready;
        if ready {
            FIXED_READY[cpu].store(true, Ordering::Release);
        }
        program_next(timers)
    })?;
    Ok(callbacks)
}

fn current_cpu() -> Result<CpuIndex, super::Error> {
    crate::cpu::current_index().ok_or(super::Error::InvalidCpuIndex)
}

/// Pins the calling continuation before selecting one CPU-local timer queue.
fn pin_current_timer_cpu() -> Result<(scheduler::PreemptionGuard, CpuIndex), super::Error> {
    let cpu_pin = scheduler::preempt_disable().map_err(super::Error::Preemption)?;
    match current_cpu() {
        Ok(cpu) => Ok((cpu_pin, cpu)),
        Err(error) => {
            release_timer_cpu_pin(cpu_pin);
            Err(error)
        }
    }
}

/// Ends a timer CPU-local transaction without adding a scheduling point.
fn release_timer_cpu_pin(cpu_pin: scheduler::PreemptionGuard) {
    if scheduler::preempt_enable_without_reschedule(cpu_pin).is_err() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: timer CPU-local preemption pin release failed"
        ));
    }
}

fn ensure_initialized(timers: &ProcessorTimers) -> Result<(), super::Error> {
    if timers.initialized {
        Ok(())
    } else {
        Err(super::Error::TimerQueueNotInitialized)
    }
}

fn program_next(timers: &ProcessorTimers) -> Result<(), super::Error> {
    match timers.next_deadline() {
        Some(deadline) => crate::hal::time::program_deadline(deadline)?,
        None => crate::hal::time::disable_local_timer(),
    }
    Ok(())
}

/// Serializes a callout's state, deadline membership, and ready membership.
/// Callout storage is permanently bound to this source CPU; the local IRQ mask
/// also prevents migration during a local comparator update.
pub(super) fn with_callout_queue<R>(
    cpu: CpuIndex,
    update: impl FnOnce(&mut OwnedDeadlineQueue, &mut super::callout::ReadyQueue) -> R,
) -> Result<R, super::Error> {
    let (result, prompt) = TIMERS[cpu].with(|timers| {
        ensure_initialized(timers)?;
        let current = current_cpu()?;
        let before = timers.queue.next_deadline();
        let result = update(&mut timers.queue, &mut timers.ready);
        let prompt = if timers.queue.next_deadline() != before {
            if current == cpu {
                if let Err(error) = program_next(timers) {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: committed callout comparator update failed: {error:?}"
                    ));
                }
                false
            } else {
                // The queue mutation is durable before the remote prompt. IRQ
                // entry consumes this flag and observes the queue under its lock.
                REPROGRAM[cpu].store(true, Ordering::Release);
                true
            }
        } else {
            false
        };
        Ok::<_, super::Error>((result, prompt))
    })?;
    if prompt {
        crate::irq::reschedule::notify(cpu);
    }
    Ok(result)
}

/// Keeps the local mask until a condition waiter commits its scheduler park.
pub(super) unsafe fn with_callout_queue_mask_retained<R>(
    cpu: CpuIndex,
    inspect: impl FnOnce() -> R,
) -> (
    R,
    hyper::sync::InterruptMaskGuard<crate::hal::irq::LocalMask>,
) {
    // SAFETY: the caller consumes the returned mask in a prepared scheduler
    // transition or drops it before returning, as this API's contract requires.
    unsafe { TIMERS[cpu].with_mask_retained(|_| inspect()) }
}

/// Reprograms a source CPU after another CPU publishes an earlier deadline.
pub(super) fn service_reprogram_prompt() -> Result<(), super::Error> {
    let cpu = current_cpu()?;
    if REPROGRAM[cpu].swap(false, Ordering::AcqRel) {
        TIMERS[cpu].with(|timers| {
            ensure_initialized(timers)?;
            program_next(timers)
        })?;
    }
    Ok(())
}

/// Delivers fixed-owner expiries after IRQ dispatch released its registry lock.
///
/// Callbacks may publish another deadline or change a local IRQ source. Neither
/// the deadline lock nor the IRQ registry lock may cross those operations.
pub(super) fn service_fixed_deadlines() -> Result<(), super::Error> {
    let cpu = current_cpu()?;
    if !FIXED_READY[cpu].swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    let (scheduler, guest_recovery) = TIMERS[cpu].with(|timers| {
        (
            timers.scheduler.take_ready(),
            timers.guest_recovery.take_ready(),
        )
    });
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    if let Some(generation) = TIMERS[cpu].with(|timers| timers.health_probe.take_ready()) {
        super::tick::validation::observe(generation)?;
    }
    if let Some(generation) = scheduler {
        scheduler::deadline_expired(generation).map_err(super::Error::Preemption)?;
    }
    if let Some(generation) = guest_recovery {
        crate::vm::recover_timer_source(generation);
    }
    Ok(())
}

impl From<TimerQueueError> for super::Error {
    fn from(error: TimerQueueError) -> Self {
        Self::TimerQueue(error)
    }
}
