// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal contracts for level-triggered object observation.

use hyper::cpu::CpuIndex;
use hyper::mm::try_box;
use hyper::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::kernel::accounting::{ResourceAmount, ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::object::{
    Event, EventError, PreparedSignalWait, SignalWaitError, SignalWaitOutcome,
};
use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::task::{WaitMobility, WaitOutcome};

static DONE: Semaphore = Semaphore::new(0);
static COMPLETION: AtomicU64 = AtomicU64::new(0);
static FAILURE: AtomicUsize = AtomicUsize::new(0);
static PRECURSOR: AtomicUsize = AtomicUsize::new(0);

const PRECURSOR_NONE: usize = 0;
const PRECURSOR_TIMEOUT: usize = 1;
const PRECURSOR_CANCEL: usize = 2;
const COMPLETION_TIMED_OUT: u64 = 2;
const COMPLETION_CANCELLED: u64 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Event,
    Resource,
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    StateMismatch(usize),
    Synchronization(crate::kernel::sync::Error),
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::kernel::sync::Error> for Error {
    fn from(error: crate::kernel::sync::Error) -> Self {
        Self::Synchronization(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Resource)?;
    let event = Event::try_new(&domain).map_err(|_| Error::Event)?;
    let state = try_box(TestState { domain, event }).map_err(|_| Error::Resource)?;

    reset_event(&state.event)?;
    exercise_signal_before_arm(&state)?;
    reset_event(&state.event)?;
    exercise_latched_observation(&state)?;
    reset_event(&state.event)?;
    exercise_resolution_before_park(
        &state,
        PRECURSOR_TIMEOUT,
        COMPLETION_TIMED_OUT,
        "object-wait/timeout-before-park",
        3,
    )?;
    exercise_resolution_before_park(
        &state,
        PRECURSOR_CANCEL,
        COMPLETION_CANCELLED,
        "object-wait/cancel-before-park",
        4,
    )?;
    exercise_sequence_exhaustion(&state.event)?;
    super::support::quiesce_workers()?;
    drop(state);
    Ok(())
}

struct TestState {
    domain: ResourceDomain,
    event: Event,
}

/// A level asserted before registration must complete the Armed generation.
fn exercise_signal_before_arm(state: &TestState) -> Result<(), Error> {
    state
        .event
        .signal(0, Event::SIGNALED.bits())
        .map_err(|_| Error::Event)?;
    start_waiter(state, "object-wait/preasserted", PRECURSOR_NONE)?;
    DONE.acquire()?;
    verify_waiter(Event::SIGNALED.bits(), 1)
}

/// Clearing a level after notification must not replace the committed snapshot.
fn exercise_latched_observation(state: &TestState) -> Result<(), Error> {
    start_waiter(state, "object-wait/latched", PRECURSOR_NONE)?;
    wait_for_registration(&state.event)?;
    state
        .event
        .signal(0, Event::SIGNALED.bits())
        .map_err(|_| Error::Event)?;
    state
        .event
        .signal(Event::SIGNALED.bits(), 0)
        .map_err(|_| Error::Event)?;
    DONE.acquire()?;
    verify_waiter(Event::SIGNALED.bits(), 2)
}

/// An exact resolver which wins while Armed must survive object publication.
fn exercise_resolution_before_park(
    state: &TestState,
    precursor: usize,
    expected: u64,
    name: &str,
    stage: usize,
) -> Result<(), Error> {
    start_waiter(state, name, precursor)?;
    DONE.acquire()?;
    verify_waiter(expected, stage)
}

/// Observation generations must exhaust without aliasing or changing level.
fn exercise_sequence_exhaustion(event: &Event) -> Result<(), Error> {
    event.set_sequence_for_test(u64::MAX);
    if event.signal(0, Event::SIGNALED.bits())
        != Err(EventError::SignalWait(SignalWaitError::SequenceExhausted))
    {
        return Err(Error::StateMismatch(31));
    }
    if event
        .observe(Event::SIGNALED.bits())
        .map_err(|_| Error::Event)?
        .is_some()
    {
        return Err(Error::StateMismatch(32));
    }
    Ok(())
}

fn start_waiter(state: &TestState, name: &str, precursor: usize) -> Result<(), Error> {
    COMPLETION.store(0, Ordering::Release);
    FAILURE.store(0, Ordering::Release);
    PRECURSOR.store(precursor, Ordering::Release);
    let waiter = scheduler::kthread_create_with_affinity(
        name,
        event_waiter,
        core::ptr::from_ref(state).expose_provenance(),
        CpuMask::single(CpuIndex::BOOT),
    )?;
    scheduler::thread_ready(waiter)?;
    Ok(())
}

extern "C" fn event_waiter(argument: usize) {
    // SAFETY: `run` owns this boxed state until DONE proves that the one-shot
    // waiter returned and scheduler quiescence reclaimed its stack.
    let state = unsafe { &*core::ptr::with_exposed_provenance::<TestState>(argument) };
    let bytes = match u64::try_from(PreparedSignalWait::allocation_size()) {
        Ok(bytes) => bytes,
        Err(_) => {
            FAILURE.store(4, Ordering::Release);
            let _ = DONE.release();
            return;
        }
    };
    let charge = state
        .domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))
        .map(|charge| charge.commit())
        .map_err(|_| ());
    let result = charge
        .and_then(|charge| {
            state
                .event
                .prepare_wait(Event::SIGNALED.bits(), charge)
                .map_err(|_| ())
        })
        .and_then(|prepared| {
            let registration = scheduler::begin_wait(WaitMobility::Migratable).map_err(|_| ())?;
            let precursor = PRECURSOR.load(Ordering::Acquire);
            let resolution = match precursor {
                PRECURSOR_NONE => None,
                PRECURSOR_TIMEOUT => Some(WaitOutcome::TimedOut),
                PRECURSOR_CANCEL => Some(WaitOutcome::Cancelled),
                _ => return Err(()),
            };
            if let Some(outcome) = resolution {
                let resolved =
                    scheduler::resolve_wait(registration.ticket(), outcome).map_err(|_| ())?;
                if !resolved.won || resolved.made_ready {
                    return Err(());
                }
            }
            state
                .event
                .wait_registered(prepared, registration)
                .map_err(|_| ())
        });
    match result {
        Ok(SignalWaitOutcome::Observed(snapshot)) if snapshot.signals() == Event::SIGNALED => {
            COMPLETION.store(snapshot.signals().bits(), Ordering::Release);
        }
        Ok(SignalWaitOutcome::TimedOut) => {
            COMPLETION.store(COMPLETION_TIMED_OUT, Ordering::Release)
        }
        Ok(SignalWaitOutcome::Cancelled) => {
            COMPLETION.store(COMPLETION_CANCELLED, Ordering::Release)
        }
        Ok(_) => FAILURE.store(1, Ordering::Release),
        Err(()) => FAILURE.store(2, Ordering::Release),
    }
    if DONE.release().is_err() {
        FAILURE.store(3, Ordering::Release);
    }
}

fn verify_waiter(expected: u64, stage: usize) -> Result<(), Error> {
    let failure = FAILURE.load(Ordering::Acquire);
    if failure != 0 {
        return Err(Error::StateMismatch(stage * 10 + failure));
    }
    if COMPLETION.load(Ordering::Acquire) != expected {
        return Err(Error::StateMismatch(stage * 10 + 4));
    }
    Ok(())
}

fn reset_event(event: &Event) -> Result<(), Error> {
    event
        .signal(Event::SIGNALED.bits(), 0)
        .map_err(|_| Error::Event)
}

fn wait_for_registration(event: &Event) -> Result<(), Error> {
    const MAX_PROGRESS_PASSES: usize = 4_096;

    for _ in 0..MAX_PROGRESS_PASSES {
        if event.waiter_count() == 1 {
            return Ok(());
        }
        scheduler::yield_now()?;
    }
    Err(Error::StateMismatch(25))
}
