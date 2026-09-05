// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal contracts for deadline-based thread blocking.

use hyper::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::kernel::task::{SleepError, sleep_ms, sleep_ns, sleep_s, sleep_until, sleep_us};

const TEST_SLEEP_NS: u64 = 2_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Clock(crate::kernel::time::Error),
    Quiescence(super::support::QuiescenceError),
    Scheduler(crate::kernel::task::scheduler::Error),
    Sleep(SleepError),
    SleptTooBriefly,
    UnexpectedSleepResult,
}

impl From<crate::kernel::time::Error> for Error {
    fn from(error: crate::kernel::time::Error) -> Self {
        Self::Clock(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

impl From<SleepError> for Error {
    fn from(error: SleepError) -> Self {
        Self::Sleep(error)
    }
}

impl From<crate::kernel::task::scheduler::Error> for Error {
    fn from(error: crate::kernel::task::scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

struct SleepObservation {
    complete: AtomicBool,
    elapsed_ns: AtomicU64,
    error: AtomicUsize,
}

impl SleepObservation {
    const fn new() -> Self {
        Self {
            complete: AtomicBool::new(false),
            elapsed_ns: AtomicU64::new(0),
            error: AtomicUsize::new(0),
        }
    }
}

static SLEEP_OBSERVATION: SleepObservation = SleepObservation::new();

pub(super) fn run() -> Result<(), Error> {
    SLEEP_OBSERVATION.complete.store(false, Ordering::Relaxed);
    SLEEP_OBSERVATION.elapsed_ns.store(0, Ordering::Relaxed);
    SLEEP_OBSERVATION.error.store(0, Ordering::Relaxed);
    let worker = crate::kernel::task::scheduler::kthread_create(
        "sleep-test",
        sleep_worker,
        core::ptr::from_ref(&SLEEP_OBSERVATION).expose_provenance(),
    )?;
    crate::kernel::task::scheduler::thread_ready(worker)?;
    while !SLEEP_OBSERVATION.complete.load(Ordering::Acquire) {
        // The bootstrap thread remains runnable while the worker parks. Timer
        // IRQ delivery eventually makes the worker eligible for this yield.
        crate::kernel::task::scheduler::yield_now()?;
    }
    if SLEEP_OBSERVATION.error.load(Ordering::Acquire) != 0 {
        return Err(Error::UnexpectedSleepResult);
    }
    if SLEEP_OBSERVATION.elapsed_ns.load(Ordering::Acquire) < TEST_SLEEP_NS {
        return Err(Error::SleptTooBriefly);
    }

    let expired = crate::kernel::time::monotonic_ticks().wrapping_sub(1);
    sleep_until(expired)?;

    crate::hal::irq::mask_local();
    let masked = sleep_ns(TEST_SLEEP_NS);
    crate::hal::irq::enable_local();
    if masked
        != Err(SleepError::Scheduler(
            crate::kernel::task::scheduler::Error::CannotSleepWithInterruptsMasked,
        ))
    {
        return Err(Error::UnexpectedSleepResult);
    }

    let guard = crate::kernel::task::scheduler::preempt_disable()?;
    let preempt_disabled = sleep_ns(TEST_SLEEP_NS);
    drop(guard);
    if preempt_disabled
        != Err(SleepError::Scheduler(
            crate::kernel::task::scheduler::Error::CannotSleepWithPreemptionDisabled,
        ))
    {
        return Err(Error::UnexpectedSleepResult);
    }

    crate::hal::irq::mask_local();
    let zero =
        sleep_ns(0).is_ok() && sleep_us(0).is_ok() && sleep_ms(0).is_ok() && sleep_s(0).is_ok();
    crate::hal::irq::enable_local();
    if !zero {
        return Err(Error::UnexpectedSleepResult);
    }
    if sleep_us(u64::MAX) != Err(SleepError::DurationOverflow)
        || sleep_ms(u64::MAX) != Err(SleepError::DurationOverflow)
        || sleep_s(u64::MAX) != Err(SleepError::DurationOverflow)
    {
        return Err(Error::UnexpectedSleepResult);
    }
    super::support::quiesce_workers()?;
    Ok(())
}

extern "C" fn sleep_worker(context: usize) {
    // SAFETY: Only the address of the static SLEEP_OBSERVATION is passed here.
    // Its lifetime dominates the one-shot worker and every failure path.
    let observation = unsafe { &*core::ptr::with_exposed_provenance::<SleepObservation>(context) };
    let start = match crate::kernel::time::monotonic_nanoseconds() {
        Ok(start) => start,
        Err(_) => {
            observation.error.store(1, Ordering::Relaxed);
            observation.complete.store(true, Ordering::Release);
            return;
        }
    };
    // A near-immediate timeout stresses expiry before or around the park
    // linearization point without relying on that race for correctness.
    if sleep_ns(1).is_err() {
        observation.error.store(2, Ordering::Relaxed);
    } else if sleep_ns(TEST_SLEEP_NS).is_err() {
        observation.error.store(3, Ordering::Relaxed);
    } else {
        match crate::kernel::time::monotonic_nanoseconds() {
            Ok(end) => observation
                .elapsed_ns
                .store(end.wrapping_sub(start), Ordering::Relaxed),
            Err(_) => observation.error.store(4, Ordering::Relaxed),
        }
    }
    observation.complete.store(true, Ordering::Release);
}
