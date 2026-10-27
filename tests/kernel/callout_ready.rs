// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic cancellation and replacement after expiry, before delivery.

use hyper::cpu::CpuIndex;
use hyper::hal::timer::deadline_reached;
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use {crate::task::scheduler, crate::task::scheduler::CpuMask};
use {
    crate::time, crate::time::Callout, crate::time::CalloutRef, crate::time::CancelOutcome,
    crate::time::TimerEvent,
};

const BLOCKER_LIMIT_NS: u64 = 5_000_000_000;
const REMOTE_DEADLINE_NS: u64 = 100_000;
const FAR_DEADLINE_NS: u64 = 60_000_000_000;

struct Blocker {
    entered: AtomicBool,
    release: AtomicBool,
    expired: AtomicBool,
}

impl Blocker {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
            expired: AtomicBool::new(false),
        }
    }
}

struct Probe {
    calls: AtomicUsize,
    drops: AtomicUsize,
}

impl Probe {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
        }
    }
}

struct OwnedContext(FallibleArc<Probe>);

impl Drop for OwnedContext {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Release);
    }
}

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
    if crate::cpu::online_cpu_count() < 2 {
        return Ok(());
    }
    let cpu = CpuIndex::new(1).ok_or(Error::Invariant("remote CPU"))?;
    let blocking = FallibleArc::try_new(Blocker::new()).map_err(|_| Error::Allocation)?;
    let blocker = Callout::try_new_on(cpu, blocking.clone(), hold_worker)?;
    let cancelled = FallibleArc::try_new(Probe::new()).map_err(|_| Error::Allocation)?;
    let reset = FallibleArc::try_new(Probe::new()).map_err(|_| Error::Allocation)?;
    let closed = FallibleArc::try_new(Probe::new()).map_err(|_| Error::Allocation)?;
    let cancel_callout = Callout::try_new_on(cpu, OwnedContext(cancelled.clone()), record)?;
    let reset_callout = Callout::try_new_on(cpu, OwnedContext(reset.clone()), record)?;
    let mut close_callout = Some(Callout::try_new_on(
        cpu,
        OwnedContext(closed.clone()),
        record,
    )?);

    let ready_checks: Result<(), Error> = (|| {
        blocker.arm_after(REMOTE_DEADLINE_NS)?;
        progress(|| blocking.entered.load(Ordering::Acquire))?;

        cancel_callout.arm_after(REMOTE_DEADLINE_NS)?;
        progress(|| cancel_callout.is_ready_for_test())?;
        if cancel_callout.cancel()? != CancelOutcome::Cancelled {
            return Err(Error::Invariant("Ready cancellation did not detach"));
        }
        cancel_callout.shutdown_sync()?;

        reset_callout.arm_after(REMOTE_DEADLINE_NS)?;
        progress(|| reset_callout.is_ready_for_test())?;
        reset_callout.reset(time::deadline_after(FAR_DEADLINE_NS)?)?;
        if reset_callout.is_ready_for_test() {
            return Err(Error::Invariant("Ready reset retained old delivery"));
        }

        let closing = close_callout
            .as_ref()
            .ok_or(Error::Invariant("close owner"))?;
        closing.arm_after(REMOTE_DEADLINE_NS)?;
        progress(|| closing.is_ready_for_test())?;
        closing.shutdown_sync()?;
        drop(close_callout.take());
        // The source worker is still inside hold_worker: deferred wake entries
        // cannot have run. Their Core owners must not retain or access T.
        if closed.drops.load(Ordering::Acquire) != 1
            || closed.calls.load(Ordering::Acquire) != 0
            || blocking.expired.load(Ordering::Acquire)
            || blocking.release.load(Ordering::Acquire)
        {
            return Err(Error::Invariant(
                "typed context retirement before deferred wake",
            ));
        }
        Ok(())
    })();

    // Every error path releases the bounded test callback before any join.
    // Its context is counted, not borrowed from this controller's stack.
    blocking.release.store(true, Ordering::Release);
    let blocker_closed = blocker.shutdown_sync();
    let cancellation_closed = cancel_callout.shutdown_sync();
    let final_checks: Result<(), Error> = (|| {
        blocker_closed?;
        cancellation_closed?;
        ready_checks?;
        if blocking.expired.load(Ordering::Acquire)
            || cancelled.calls.load(Ordering::Acquire) != 0
            || reset.calls.load(Ordering::Acquire) != 0
            || closed.calls.load(Ordering::Acquire) != 0
        {
            return Err(Error::Invariant("suppressed Ready callback executed"));
        }
        reset_callout.reset(time::deadline_after(REMOTE_DEADLINE_NS)?)?;
        progress(|| reset.calls.load(Ordering::Acquire) == 1)?;
        Ok(())
    })();
    let reset_closed = reset_callout.shutdown_sync();
    let owner_closed = match close_callout.as_ref() {
        Some(callout) => callout.shutdown_sync(),
        None => Ok(()),
    };
    reset_closed?;
    owner_closed?;
    final_checks?;
    // A stable blocked worker has consumed both its callback and wake FIFOs,
    // including the retained Core whose typed owner was dropped above.
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    if reset.calls.load(Ordering::Acquire) != 1
        || cancelled.calls.load(Ordering::Acquire) != 0
        || closed.calls.load(Ordering::Acquire) != 0
    {
        return Err(Error::Invariant("callback escaped Ready cancellation"));
    }
    crate::pr_info!(
        "HypeR test: Ready callout cancellation, replacement and owner retirement passed"
    );
    exercise_running_shutdown()?;
    Ok(())
}

struct RunningProbe {
    entered: AtomicBool,
    release: AtomicBool,
    exited: AtomicBool,
    saw_closed: AtomicBool,
    failed: AtomicBool,
}

#[derive(Clone)]
struct RunningControl {
    callout: FallibleArc<Callout<FallibleArc<RunningProbe>>>,
    probe: FallibleArc<RunningProbe>,
}

static RUNNING: InterruptSpinLock<Option<RunningControl>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);

fn exercise_running_shutdown() -> Result<(), Error> {
    if crate::cpu::online_cpu_count() < 3 {
        return Ok(());
    }
    let source = CpuIndex::new(1).ok_or(Error::Invariant("source CPU"))?;
    let releaser_cpu = CpuIndex::new(2).ok_or(Error::Invariant("releaser CPU"))?;
    let probe = FallibleArc::try_new(RunningProbe {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
        exited: AtomicBool::new(false),
        saw_closed: AtomicBool::new(false),
        failed: AtomicBool::new(false),
    })
    .map_err(|_| Error::Allocation)?;
    let callout = FallibleArc::try_new(Callout::try_new_on(source, probe.clone(), hold_running)?)
        .map_err(|_| Error::Allocation)?;
    RUNNING.with(|slot| {
        *slot = Some(RunningControl {
            callout: callout.clone(),
            probe: probe.clone(),
        })
    });
    let check: Result<(), Error> = (|| {
        let releaser = scheduler::kthread_create_with_affinity(
            "timer/release",
            release_after_close,
            0,
            CpuMask::single(releaser_cpu),
        )?;
        if !scheduler::thread_ready(releaser)? {
            return Err(Error::Invariant("releaser publication"));
        }
        callout.arm_after(REMOTE_DEADLINE_NS)?;
        progress(|| probe.entered.load(Ordering::Acquire))?;
        if probe.exited.load(Ordering::Acquire) {
            return Err(Error::Invariant("running callback ended before shutdown"));
        }
        // The releaser observes Closed through reset(), then lets this callback
        // return. No delay guess or private shutdown-state probe is involved.
        callout.shutdown_sync()?;
        if !probe.saw_closed.load(Ordering::Acquire)
            || !probe.exited.load(Ordering::Acquire)
            || probe.failed.load(Ordering::Acquire)
        {
            return Err(Error::Invariant("shutdown failed to join Running callback"));
        }
        Ok(())
    })();
    probe.release.store(true, Ordering::Release);
    let closed = callout.shutdown_sync();
    // The counted static record remains available until the transient
    // releaser's execution and stack have retired, including assertion errors.
    let quiescent = super::support::quiesce_workers().map_err(Error::Quiescence);
    if quiescent.is_ok() {
        let retained = RUNNING.with(Option::take);
        drop(retained);
    }
    closed?;
    quiescent?;
    check?;
    crate::pr_info!("HypeR test: shutdown admission and Running callout join passed");
    Ok(())
}

fn hold_running(probe: &FallibleArc<RunningProbe>, _: CalloutRef<'_>, _: TimerEvent) {
    let deadline = match time::deadline_after(BLOCKER_LIMIT_NS) {
        Ok(deadline) => deadline,
        Err(_) => {
            probe.failed.store(true, Ordering::Release);
            probe.entered.store(true, Ordering::Release);
            probe.exited.store(true, Ordering::Release);
            return;
        }
    };
    probe.entered.store(true, Ordering::Release);
    while !probe.release.load(Ordering::Acquire) {
        if deadline_reached(time::monotonic_ticks(), deadline) {
            probe.failed.store(true, Ordering::Release);
            break;
        }
        core::hint::spin_loop();
    }
    probe.exited.store(true, Ordering::Release);
}

extern "C" fn release_after_close(_: usize) {
    let Some(control) = RUNNING.with(|slot| slot.clone()) else {
        return;
    };
    let deadline = match time::deadline_after(BLOCKER_LIMIT_NS) {
        Ok(deadline) => deadline,
        Err(_) => {
            control.probe.failed.store(true, Ordering::Release);
            control.probe.release.store(true, Ordering::Release);
            return;
        }
    };
    while !control.probe.release.load(Ordering::Acquire) {
        if control.probe.entered.load(Ordering::Acquire) {
            match time::deadline_after(FAR_DEADLINE_NS)
                .and_then(|deadline| control.callout.reset(deadline))
            {
                Err(time::Error::CalloutClosed) => {
                    control.probe.saw_closed.store(true, Ordering::Release);
                    control.probe.release.store(true, Ordering::Release);
                    return;
                }
                Ok(()) => {}
                Err(_) => break,
            }
        }
        if deadline_reached(time::monotonic_ticks(), deadline) || scheduler::cond_resched().is_err()
        {
            break;
        }
        core::hint::spin_loop();
    }
    if !control.probe.release.load(Ordering::Acquire) {
        control.probe.failed.store(true, Ordering::Release);
        control.probe.release.store(true, Ordering::Release);
    }
}

fn hold_worker(blocker: &FallibleArc<Blocker>, _: CalloutRef<'_>, _: TimerEvent) {
    let deadline = match time::deadline_after(BLOCKER_LIMIT_NS) {
        Ok(deadline) => deadline,
        Err(_) => {
            blocker.expired.store(true, Ordering::Release);
            blocker.entered.store(true, Ordering::Release);
            return;
        }
    };
    blocker.entered.store(true, Ordering::Release);
    // Test-only bounded work opens the Ready window without sleeping or
    // masking IRQs: the source timer can still enqueue other expirations.
    while !blocker.release.load(Ordering::Acquire) {
        if deadline_reached(time::monotonic_ticks(), deadline) {
            blocker.expired.store(true, Ordering::Release);
            return;
        }
        core::hint::spin_loop();
    }
}

fn record(context: &OwnedContext, _: CalloutRef<'_>, _: TimerEvent) {
    context.0.calls.fetch_add(1, Ordering::Release);
}

fn progress(mut condition: impl FnMut() -> bool) -> Result<(), Error> {
    let done = crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, crate::task::SleepError>(condition())
    })
    .map_err(|_| Error::Progress)?;
    if done { Ok(()) } else { Err(Error::Progress) }
}
