// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host clocksource, timer queues, and one-shot architectural clock events.
//!
//! This subsystem owns time conversion and per-CPU deadline multiplexing. It consumes an
//! IRQ domain for physical delivery but does not own interrupt-controller
//! discovery or routing policy.

mod callout;
mod deadline_slot;
mod realtime;
mod tick;
mod timers;
mod worker;

#[allow(
    unused_imports,
    reason = "cancellation outcomes are part of the callout API"
)]
pub(crate) use callout::CancelOutcome;
pub(crate) use callout::{Callout, CalloutRef};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use worker::permanent_worker_count_for_test;
pub(crate) use worker::{initialize as initialize_workers, service_irq_prompt};

pub(crate) use realtime::initialize as initialize_realtime;
pub(crate) use realtime::now as realtime;

pub(crate) use tick::Error as TickError;

pub mod queue;

use core::hint::spin_loop;

#[allow(
    unused_imports,
    reason = "owned timer guards serve the RISC-V guest timer"
)]
pub(crate) use timers::OwnedArmedIrqCallout;
#[cfg(all(
    target_arch = "aarch64",
    feature = "kernel-test",
    feature = "kernel-self-test"
))]
pub(crate) use timers::scheduler_deadline_for_test;
pub(crate) use timers::{ArmedIrqCallout, IrqCallout};
pub use timers::{
    QueueStats as TimerQueueStats, TimerCallback, TimerEvent, TimerHandle, TimerMode,
    cancel_irq_timer, local_statistics as timer_statistics, schedule_irq_after, schedule_irq_at,
    schedule_irq_periodic,
};
pub(crate) use timers::{
    publish_guest_timer_recovery, publish_scheduler_deadline, service_scheduler_deadline,
};

use hyper::hal::interrupt::InterruptId;
use hyper::hal::timer::{
    ConversionError, deadline_reached, nanoseconds_to_ticks, ticks_to_nanoseconds,
};
use hyper::sync::atomic::{AtomicU64, Ordering};

use crate::irq::interrupt::VirtualInterrupt;

static COUNTER_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyInitialized,
    Architecture(crate::hal::time::Error),
    Conversion(ConversionError),
    DeadlineTooFar,
    InvalidCpuIndex,
    NotInitialized,
    Preemption(crate::task::scheduler::Error),
    TimerQueue(crate::time::queue::TimerQueueError),
    TimerQueueAlreadyInitialized,
    TimerQueueNotInitialized,
    CalloutClosed,
    CalloutBusy,
    CalloutGenerationExhausted,
    CalloutWorkerUnavailable,
    CalloutCallbackContext,
    CalloutWait(crate::task::scheduler::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    Clock(Error),
    MissingTimer,
    Tick(tick::Error),
}

impl From<crate::hal::time::Error> for Error {
    fn from(error: crate::hal::time::Error) -> Self {
        Self::Architecture(error)
    }
}

impl From<ConversionError> for Error {
    fn from(error: ConversionError) -> Self {
        Self::Conversion(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuestTimerSource {
    pub(crate) interrupt: InterruptId,
    pub(crate) requires_host_mapping: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Capabilities {
    pub(crate) counter_frequency_hz: u64,
    pub(crate) hardware_interrupt: InterruptId,
    pub(crate) virtual_interrupt: VirtualInterrupt,
    pub(crate) guest_timer: GuestTimerSource,
}

fn initialize_clock() -> Result<u64, Error> {
    let frequency = crate::hal::time::counter_frequency_hz()?;
    COUNTER_FREQUENCY_HZ
        .compare_exchange(0, frequency, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| Error::AlreadyInitialized)?;
    Ok(frequency)
}

/// Initializes the monotonic clock, architectural comparator, and boot CPU queue.
pub(crate) fn initialize(
    boot: &mut crate::boot::Initialization,
) -> Result<(), InitializationError> {
    crate::hal::time::prepare(boot.essential())
        .map_err(Error::Architecture)
        .map_err(InitializationError::Clock)?;
    let counter_frequency_hz = initialize_clock().map_err(InitializationError::Clock)?;
    crate::pr_info!(
        "HypeR: monotonic clocksource active at {} Hz",
        counter_frequency_hz
    );

    let info = boot
        .essential()
        .timer()
        .ok_or(InitializationError::MissingTimer)?;
    let capabilities =
        tick::initialize(info, boot.interrupts().root_domain).map_err(InitializationError::Tick)?;
    crate::pr_info!(
        "HypeR: architectural timer: host INTID {}, guest INTID {}, one-shot from a {} Hz counter",
        capabilities.hardware_interrupt.get(),
        capabilities.guest_timer.interrupt.get(),
        capabilities.counter_frequency_hz
    );
    crate::pr_info!(
        "HypeR: timer mapped to dynamic VIRQ {}",
        capabilities.virtual_interrupt.get()
    );
    crate::pr_info!("HypeR: dynamically owned per-CPU software timer queues active");
    boot.set_timer(capabilities);
    Ok(())
}

pub(crate) fn initialize_local_cpu() -> Result<(), tick::Error> {
    tick::initialize_local_cpu()
}

pub(crate) fn initialize_local_timer_queue() -> Result<(), Error> {
    timers::initialize_local()
}

pub(crate) fn handle_timer_interrupt() -> Result<usize, Error> {
    timers::handle_interrupt()
}

pub fn counter_frequency_hz() -> Result<u64, Error> {
    let frequency = COUNTER_FREQUENCY_HZ.load(Ordering::Acquire);
    if frequency == 0 {
        Err(Error::NotInitialized)
    } else {
        Ok(frequency)
    }
}

pub fn monotonic_ticks() -> u64 {
    crate::hal::time::read_counter()
}

/// Returns microseconds elapsed since the primary assembly entry.
///
/// Early logging is allowed before clocksource discovery; those records use
/// zero until the counter frequency has been published.
pub fn monotonic_microseconds() -> u64 {
    let frequency = COUNTER_FREQUENCY_HZ.load(Ordering::Acquire);
    if frequency == 0 {
        return 0;
    }
    crate::time::queue::counter_elapsed_microseconds(
        monotonic_ticks(),
        crate::hal::time::boot_counter(),
        frequency,
    )
}

pub fn monotonic_nanoseconds() -> Result<u64, Error> {
    Ok(ticks_to_nanoseconds(
        monotonic_ticks(),
        counter_frequency_hz()?,
    )?)
}

/// Returns an absolute counter deadline no earlier than `nanoseconds` ahead.
pub fn deadline_after(nanoseconds: u64) -> Result<u64, Error> {
    let ticks = nanoseconds_to_ticks(nanoseconds, counter_frequency_hz()?)?;
    if ticks > i64::MAX as u64 {
        return Err(Error::DeadlineTooFar);
    }
    Ok(monotonic_ticks().wrapping_add(ticks))
}

/// Converts an absolute monotonic nanosecond value into a counter deadline.
///
/// The counter and its nanosecond representation are sampled once. Deriving
/// the delta and final deadline from that same snapshot prevents syscall
/// preparation time from silently extending an absolute deadline.
pub(crate) fn deadline_from_monotonic_nanoseconds(nanoseconds: u64) -> Result<u64, Error> {
    let frequency = counter_frequency_hz()?;
    let now_ticks = monotonic_ticks();
    let now_nanoseconds = ticks_to_nanoseconds(now_ticks, frequency)?;
    if nanoseconds <= now_nanoseconds {
        return Ok(now_ticks);
    }
    let delta_ticks = nanoseconds_to_ticks(nanoseconds - now_nanoseconds, frequency)?;
    if delta_ticks > i64::MAX as u64 {
        return Err(Error::DeadlineTooFar);
    }
    Ok(now_ticks.wrapping_add(delta_ticks))
}

/// Polls an allocation-free condition until it succeeds or monotonic time
/// reaches the requested duration.
///
/// This is intended for boot and fail-stop handshakes where scheduling or
/// interrupts may be unavailable. Normal runtime waits should block a Thread.
pub(crate) fn spin_wait_until(
    timeout_nanoseconds: u64,
    mut condition: impl FnMut() -> bool,
) -> Result<bool, Error> {
    let deadline = deadline_after(timeout_nanoseconds)?;
    loop {
        if condition() {
            return Ok(true);
        }
        if deadline_reached(monotonic_ticks(), deadline) {
            return Ok(false);
        }
        spin_loop();
    }
}
