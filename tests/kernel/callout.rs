// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real timer-worker delivery, self-rearming, and cross-CPU close races.

use hyper::cpu::CpuIndex;
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::time::{self, Callout, CalloutRef, CancelOutcome, TimerEvent};

const RACE_ROUNDS: usize = 256;
const SELF_REARMS: usize = 8;

struct Probe {
    cpu: CpuIndex,
    callbacks: AtomicUsize,
    active: AtomicBool,
    failed: AtomicBool,
    repeat: bool,
}

impl Probe {
    fn new(cpu: CpuIndex, repeat: bool) -> Self {
        Self {
            cpu,
            callbacks: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            repeat,
        }
    }
}

type TestCallout = Callout<FallibleArc<Probe>>;
static RACER: InterruptSpinLock<Option<FallibleArc<TestCallout>>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);
static START: AtomicBool = AtomicBool::new(false);
static ACTORS_READY: AtomicUsize = AtomicUsize::new(0);
static RACE_FAILED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Allocation,
    Time(time::Error),
    Scheduler(scheduler::Error),
    Progress,
    Invariant(&'static str),
    Quiescence(super::support::QuiescenceError),
}

impl From<time::Error> for Error {
    fn from(error: time::Error) -> Self {
        Self::Time(error)
    }
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    for index in 0..crate::kernel::cpu::online_cpu_count() {
        let cpu = CpuIndex::new(index).ok_or(Error::Invariant("CPU identity"))?;
        exercise_delivery(cpu)?;
    }
    exercise_self_rearm()?;
    exercise_worker_wait_rejection()?;
    exercise_close_race()?;
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    crate::pr_info!("HypeR test: timer-worker delivery, rearm and shutdown races passed");
    Ok(())
}

fn exercise_delivery(cpu: CpuIndex) -> Result<(), Error> {
    let probe = FallibleArc::try_new(Probe::new(cpu, false)).map_err(|_| Error::Allocation)?;
    let callout = Callout::try_new_on(cpu, probe.clone(), callback)?;
    let result: Result<(), Error> = (|| {
        callout.arm_after(10_000_000_000)?;
        if callout.cancel()? != CancelOutcome::Cancelled {
            return Err(Error::Invariant("queued cancellation"));
        }
        callout.arm_after(10_000_000_000)?;
        // Also exercises remote comparator prompting: the owning CPU is
        // explicit, while this caller remains the bootstrap Thread on CPU0.
        callout.reset(time::deadline_after(100_000)?)?;
        progress(|| probe.callbacks.load(Ordering::Acquire) == 1)?;
        callout.drain()?;
        if probe.failed.load(Ordering::Acquire) || probe.active.load(Ordering::Acquire) {
            return Err(Error::Invariant("callback execution context"));
        }
        if callout.cancel()? != CancelOutcome::Idle {
            return Err(Error::Invariant("completed cancellation"));
        }
        Ok(())
    })();
    // Close even on assertion failure so no callback can outlive the probe.
    callout.shutdown_sync()?;
    result?;
    if callout.arm_after(1) != Err(time::Error::CalloutClosed) {
        return Err(Error::Invariant("closed arm admitted"));
    }
    Ok(())
}

fn exercise_self_rearm() -> Result<(), Error> {
    let cpu = CpuIndex::BOOT;
    let probe = FallibleArc::try_new(Probe::new(cpu, true)).map_err(|_| Error::Allocation)?;
    let callout = Callout::try_new_on(cpu, probe.clone(), callback)?;
    let result: Result<(), Error> = (|| {
        callout.arm_after(100_000)?;
        progress(|| probe.callbacks.load(Ordering::Acquire) == SELF_REARMS)?;
        callout.drain()?;
        if probe.failed.load(Ordering::Acquire) || probe.active.load(Ordering::Acquire) {
            return Err(Error::Invariant("self-rearm overlap"));
        }
        Ok(())
    })();
    callout.shutdown_sync()?;
    result
}

fn callback(probe: &FallibleArc<Probe>, callout: CalloutRef<'_>, _: TimerEvent) {
    if probe.active.swap(true, Ordering::AcqRel)
        || crate::kernel::cpu::current_index() != Some(probe.cpu)
        || !crate::hal::irq::local_enabled()
        || scheduler::ensure_sleepable().is_err()
    {
        probe.failed.store(true, Ordering::Release);
    }
    let count = probe.callbacks.fetch_add(1, Ordering::AcqRel) + 1;
    if count == 1 && verify_worker_identity(probe.cpu).is_err() {
        probe.failed.store(true, Ordering::Release);
    }
    if probe.repeat && count < SELF_REARMS {
        let rearm = if count.is_multiple_of(2) {
            callout.arm_after(100_000)
        } else {
            time::deadline_after(100_000).and_then(|deadline| callout.reset(deadline))
        };
        match rearm {
            Ok(()) | Err(time::Error::CalloutClosed) => {}
            Err(_) => probe.failed.store(true, Ordering::Release),
        }
    } else if probe.repeat && callout.cancel() != Ok(CancelOutcome::Running) {
        probe.failed.store(true, Ordering::Release);
    }
    probe.active.store(false, Ordering::Release);
}

fn verify_worker_identity(cpu: CpuIndex) -> Result<(), Error> {
    let thread = scheduler::current_thread_id()?;
    let name = scheduler::thread_name(thread)?;
    if name
        .as_str()
        .strip_prefix("ktimer/")
        .and_then(|suffix| suffix.parse::<usize>().ok())
        != Some(cpu.get())
        || scheduler::thread_placement(thread)? != (cpu, CpuMask::single(cpu))
    {
        return Err(Error::Invariant("timer worker identity and ownership"));
    }
    // A singleton mask alone would pass the placement check above. CPU-owned
    // workers must also reject attempts to turn them into movable Threads.
    if scheduler::set_thread_affinity(thread, CpuMask::ALL)
        != Err(scheduler::Error::MigrationUnsupported)
        || scheduler::migrate_thread(thread, cpu) != Err(scheduler::Error::MigrationUnsupported)
    {
        return Err(Error::Invariant("timer worker accepted placement changes"));
    }
    Ok(())
}

struct WaitProbe {
    other: Callout<()>,
    result: FallibleArc<AtomicUsize>,
}

fn exercise_worker_wait_rejection() -> Result<(), Error> {
    let result = FallibleArc::try_new(AtomicUsize::new(0)).map_err(|_| Error::Allocation)?;
    let other = Callout::try_new((), |_, _, _| {})?;
    let callout = Callout::try_new(
        WaitProbe {
            other,
            result: result.clone(),
        },
        |probe, _, _| {
            // Even joining another inactive callout must reject this context:
            // allowing it conditionally would make future rearming introduce
            // an unsuspected cycle between the same persistent workers.
            let rejected = probe.other.drain() == Err(time::Error::CalloutCallbackContext)
                && probe.other.shutdown_sync() == Err(time::Error::CalloutCallbackContext);
            probe
                .result
                .store(if rejected { 1 } else { 2 }, Ordering::Release);
        },
    )?;
    let check = (|| {
        callout.arm_after(100_000)?;
        progress(|| result.load(Ordering::Acquire) != 0)
    })();
    callout.shutdown_sync()?;
    check?;
    if result.load(Ordering::Acquire) != 1 {
        return Err(Error::Invariant("timer worker synchronous wait admitted"));
    }
    Ok(())
}

fn exercise_close_race() -> Result<(), Error> {
    let online = crate::kernel::cpu::online_cpu_count();
    if online < 3 {
        return Ok(());
    }
    let owner = CpuIndex::new(1).ok_or(Error::Invariant("owner CPU"))?;
    let probe = FallibleArc::try_new(Probe::new(owner, false)).map_err(|_| Error::Allocation)?;
    let callout = FallibleArc::try_new(Callout::try_new_on(owner, probe.clone(), callback)?)
        .map_err(|_| Error::Allocation)?;
    START.store(false, Ordering::Relaxed);
    ACTORS_READY.store(0, Ordering::Relaxed);
    RACE_FAILED.store(false, Ordering::Relaxed);
    RACER.with(|slot| *slot = Some(callout.clone()));
    let actors = (online - 1).min(3);
    for index in 1..=actors {
        let cpu = CpuIndex::new(index).ok_or(Error::Invariant("racer CPU"))?;
        let worker = scheduler::kthread_create_with_affinity(
            "timer/racer",
            race_worker,
            index,
            CpuMask::single(cpu),
        )?;
        scheduler::thread_ready(worker)?;
    }
    let result: Result<(), Error> = (|| {
        progress(|| ACTORS_READY.load(Ordering::Acquire) == actors)?;
        START.store(true, Ordering::Release);
        for _ in 0..32 {
            callout.reset(time::deadline_after(1)?)?;
            let _ = callout.cancel()?;
            scheduler::cond_resched()?;
        }
        Ok(())
    })();
    START.store(true, Ordering::Release);
    callout.shutdown_sync()?;
    let count_at_close = probe.callbacks.load(Ordering::Acquire);
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    let retained = RACER.with(Option::take);
    drop(retained);
    result?;
    if RACE_FAILED.load(Ordering::Acquire)
        || probe.failed.load(Ordering::Acquire)
        || probe.active.load(Ordering::Acquire)
        || probe.callbacks.load(Ordering::Acquire) != count_at_close
    {
        return Err(Error::Invariant("reset/cancel/shutdown race"));
    }
    Ok(())
}

extern "C" fn race_worker(index: usize) {
    // The static slot and this counted owner survive all controller errors;
    // quiescence, rather than a completion flag, permits final retirement.
    let callout = match RACER.with(|slot| slot.clone()) {
        Some(callout) => callout,
        None => {
            RACE_FAILED.store(true, Ordering::Release);
            return;
        }
    };
    ACTORS_READY.fetch_add(1, Ordering::Release);
    while !START.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    for round in 0..RACE_ROUNDS {
        let result = if (round + index).is_multiple_of(3) {
            callout.cancel().map(|_| ())
        } else {
            time::deadline_after(1).and_then(|deadline| callout.reset(deadline))
        };
        if !matches!(result, Ok(()) | Err(time::Error::CalloutClosed)) {
            RACE_FAILED.store(true, Ordering::Release);
        }
        if scheduler::cond_resched().is_err() {
            RACE_FAILED.store(true, Ordering::Release);
            return;
        }
    }
}

fn progress(mut condition: impl FnMut() -> bool) -> Result<(), Error> {
    let complete = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, crate::kernel::task::SleepError>(condition()),
    )
    .map_err(|_| Error::Progress)?;
    if complete {
        Ok(())
    } else {
        Err(Error::Progress)
    }
}
