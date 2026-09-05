// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic contracts for wait registration arbitration and mobility.

use hyper::cpu::CpuIndex;
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::task::{WaitMobility, WaitOutcome, WaitQueue, WaitTicket};

static CPU_LOCAL_DONE: Semaphore = Semaphore::new(0);
static CPU_LOCAL_FAILURE: AtomicUsize = AtomicUsize::new(0);
static TIMED_QUEUE: WaitQueue = WaitQueue::new();
static TIMED_DONE: Semaphore = Semaphore::new(0);
static TIMED_OUTCOME: AtomicUsize = AtomicUsize::new(0);

type ArbitrationLock = InterruptSpinLock<ArbitrationState, crate::hal::irq::LocalMask>;

struct ArbitrationState {
    queue: WaitQueue,
    published_ticket: Option<WaitTicket>,
    committed_observation: usize,
}

struct QueuedArbitration {
    state: ArbitrationLock,
    done: Semaphore,
    outcomes: AtomicUsize,
    failure: AtomicUsize,
}

impl QueuedArbitration {
    const fn new() -> Self {
        Self {
            state: ArbitrationLock::new(ArbitrationState {
                queue: WaitQueue::new(),
                published_ticket: None,
                committed_observation: 0,
            }),
            done: Semaphore::new(0),
            outcomes: AtomicUsize::new(0),
            failure: AtomicUsize::new(0),
        }
    }
}

static QUEUED_ARBITRATION: QueuedArbitration = QueuedArbitration::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    Synchronization(crate::kernel::sync::Error),
    TimedWait(crate::kernel::task::TimedWaitError),
    StateMismatch(usize),
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::kernel::task::SleepError> for Error {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

impl From<crate::kernel::sync::Error> for Error {
    fn from(error: crate::kernel::sync::Error) -> Self {
        Self::Synchronization(error)
    }
}

impl From<crate::kernel::task::TimedWaitError> for Error {
    fn from(error: crate::kernel::task::TimedWaitError) -> Self {
        Self::TimedWait(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    exercise_exact_notification()?;
    exercise_unqueued_arbitration()?;
    exercise_queued_arbitration_and_stale_ticket()?;
    exercise_timed_wait_paths()?;
    exercise_cpu_local_migration_barrier()?;
    super::support::quiesce_workers()?;
    Ok(())
}

/// Proves that an exact notification commits caller-owned payload only for
/// the winning generation, including while the Thread is still Armed.
fn exercise_exact_notification() -> Result<(), Error> {
    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
    let ticket = registration.ticket();
    let mut observation = 0usize;
    let winner = scheduler::notify_registered_with(ticket, || observation = 0x51)?;
    let loser = scheduler::notify_registered_with(ticket, || observation = 0x99)?;
    if !winner.won || winner.made_ready || loser.won || loser.made_ready || observation != 0x51 {
        return Err(Error::StateMismatch(26));
    }
    if scheduler::finish_wait(registration)? != Some(WaitOutcome::Notified) {
        return Err(Error::StateMismatch(27));
    }
    let stale = scheduler::notify_registered_with(ticket, || observation = 0xff)?;
    if stale.won || stale.made_ready || observation != 0x51 {
        return Err(Error::StateMismatch(28));
    }
    Ok(())
}

/// Exercises the real timer callback and handle-directed retirement paths.
fn exercise_timed_wait_paths() -> Result<(), Error> {
    let immediate = WaitQueue::new();
    if immediate.wait_for(1)? != WaitOutcome::TimedOut {
        return Err(Error::StateMismatch(12));
    }

    TIMED_OUTCOME.store(0, Ordering::Release);
    let source = if crate::kernel::cpu::online_cpu_count() > 1 {
        CpuIndex::new(1).ok_or(Error::StateMismatch(13))?
    } else {
        CpuIndex::BOOT
    };
    let worker = scheduler::kthread_create_with_affinity(
        "wait/timed-notification",
        timed_wait_worker,
        0,
        CpuMask::single(source),
    )?;
    if source != CpuIndex::BOOT {
        let migratable = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(source);
        if scheduler::set_thread_affinity(worker, migratable)?
            != scheduler::MigrationStatus::Completed
        {
            return Err(Error::StateMismatch(25));
        }
    }
    scheduler::thread_ready(worker)?;
    wait_for_timed_waiter()?;

    // Moving the blocked Thread away from the timer's source CPU forces the
    // resumed waiter to retire the handle from a remote per-CPU queue.
    if source != CpuIndex::BOOT {
        // Queue publication may become visible before the source CPU's
        // incoming switch tail releases `switching_from`. Both Completed and
        // Pending mean the migration transaction was accepted; in the latter
        // case that tail moves a concurrently awakened Ready thread before it
        // can execute, preserving the remote timer-retirement proof.
        match scheduler::migrate_thread(worker, CpuIndex::BOOT)? {
            scheduler::MigrationStatus::Completed | scheduler::MigrationStatus::Pending => {}
        }
    }
    if TIMED_QUEUE.wake_one()? != Some(worker) {
        return Err(Error::StateMismatch(15));
    }
    wait_for_completion(&TIMED_DONE, 21)?;
    if TIMED_OUTCOME.load(Ordering::Acquire) != 1 {
        return Err(Error::StateMismatch(16));
    }

    TIMED_OUTCOME.store(0, Ordering::Release);
    let cancelled = scheduler::kthread_create_with_affinity(
        "wait/timed-cancellation",
        timed_wait_worker,
        0,
        CpuMask::single(CpuIndex::BOOT),
    )?;
    scheduler::thread_ready(cancelled)?;
    wait_for_timed_waiter()?;
    if !TIMED_QUEUE.cancel(cancelled)? {
        return Err(Error::StateMismatch(17));
    }
    wait_for_completion(&TIMED_DONE, 22)?;
    if TIMED_OUTCOME.load(Ordering::Acquire) != 3 {
        return Err(Error::StateMismatch(18));
    }
    Ok(())
}

fn wait_for_timed_waiter() -> Result<(), Error> {
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(TIMED_QUEUE.len()? == 1),
    )? {
        Ok(())
    } else {
        Err(Error::StateMismatch(19))
    }
}

extern "C" fn timed_wait_worker(_argument: usize) {
    let observed = match TIMED_QUEUE.wait_for(10_000_000_000) {
        Ok(WaitOutcome::Notified) => 1,
        Ok(WaitOutcome::TimedOut) => 2,
        Ok(WaitOutcome::Cancelled) => 3,
        Err(_) => 4,
    };
    TIMED_OUTCOME.store(observed, Ordering::Release);
    if TIMED_DONE.release().is_err() {
        TIMED_OUTCOME.store(5, Ordering::Release);
    }
}

/// Resolves an Armed registration before queue publication.
///
/// The first resolver owns the terminal outcome. A competing resolver and a
/// replay after retirement must both lose without changing later wait state.
fn exercise_unqueued_arbitration() -> Result<(), Error> {
    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
    let ticket = registration.ticket();
    let winner = scheduler::resolve_wait(ticket, WaitOutcome::Cancelled)?;
    let loser = scheduler::resolve_wait(ticket, WaitOutcome::TimedOut)?;
    if !winner.won || winner.made_ready || loser.won || loser.made_ready {
        return Err(Error::StateMismatch(1));
    }
    if scheduler::finish_wait(registration)? != Some(WaitOutcome::Cancelled) {
        return Err(Error::StateMismatch(2));
    }
    let stale = scheduler::resolve_wait(ticket, WaitOutcome::TimedOut)?;
    if stale.won || stale.made_ready {
        return Err(Error::StateMismatch(3));
    }

    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
    let ticket = registration.ticket();
    if scheduler::finish_wait(registration)?.is_some() {
        return Err(Error::StateMismatch(4));
    }
    let stale = scheduler::resolve_wait(ticket, WaitOutcome::Cancelled)?;
    if stale.won || stale.made_ready {
        return Err(Error::StateMismatch(5));
    }
    Ok(())
}

/// Runs two waits on one Thread and replays the first generation.
///
/// The controller resolves each wait only after queue publication. The second
/// round proves that the first ticket cannot cancel a later wait on the same
/// Thread and queue. Selecting CPU 1 when available also exercises remote
/// ready publication; a one-CPU system executes the identical protocol after a
/// bounded scheduler-aware wait.
fn exercise_queued_arbitration_and_stale_ticket() -> Result<(), Error> {
    let state = &QUEUED_ARBITRATION;
    state.outcomes.store(0, Ordering::Release);
    state.failure.store(0, Ordering::Release);
    state.state.with(|inner| {
        inner.published_ticket = None;
        inner.committed_observation = 0;
    });

    let worker_cpu = if crate::kernel::cpu::online_cpu_count() > 1 {
        CpuIndex::new(1).ok_or(Error::StateMismatch(6))?
    } else {
        CpuIndex::BOOT
    };
    let worker = scheduler::kthread_create_with_affinity(
        "wait/queued-arbitration",
        queued_arbitration_worker,
        0,
        CpuMask::single(worker_cpu),
    )?;
    scheduler::thread_ready(worker)?;

    let first = wait_for_published_ticket(None)?;
    let first_resolution = state.state.with(|inner| {
        scheduler::notify_registered_with(first, || inner.committed_observation = 0x71)
    })?;
    if !first_resolution.won || !first_resolution.made_ready {
        return Err(Error::StateMismatch(7));
    }
    let replay = state.state.with(|inner| {
        scheduler::notify_registered_with(first, || inner.committed_observation = 0x99)
    })?;
    if replay.won
        || replay.made_ready
        || state
            .state
            .with(|inner| inner.committed_observation != 0x71)
    {
        return Err(Error::StateMismatch(29));
    }

    let second = wait_for_published_ticket(Some(first))?;
    if second == first {
        return Err(Error::StateMismatch(8));
    }
    let stale = scheduler::resolve_wait(first, WaitOutcome::Cancelled)?;
    let unrelated = WaitQueue::new();
    if stale.won || stale.made_ready || unrelated.cancel(worker)? {
        return Err(Error::StateMismatch(9));
    }
    if !state.state.with(|inner| inner.queue.cancel(worker))? {
        return Err(Error::StateMismatch(10));
    }

    wait_for_completion(&state.done, 23)?;
    let failure = state.failure.load(Ordering::Acquire);
    if failure != 0 {
        return Err(Error::StateMismatch(200 + failure));
    }
    if state.outcomes.load(Ordering::Acquire) != 0b11 {
        return Err(Error::StateMismatch(11));
    }
    Ok(())
}

fn wait_for_published_ticket(previous: Option<WaitTicket>) -> Result<WaitTicket, Error> {
    let mut observed = None;
    let published = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || {
            let ticket = QUEUED_ARBITRATION
                .state
                .with(|state| state.published_ticket);
            if ticket.is_some() && ticket != previous {
                observed = ticket;
                return Ok::<_, Error>(true);
            }
            Ok(false)
        },
    )?;
    if published {
        observed.ok_or(Error::StateMismatch(20))
    } else {
        Err(Error::StateMismatch(20))
    }
}

extern "C" fn queued_arbitration_worker(_argument: usize) {
    match park_for_test() {
        Ok(WaitOutcome::Notified) => {
            if QUEUED_ARBITRATION
                .state
                .with(|state| state.committed_observation)
                != 0x71
            {
                QUEUED_ARBITRATION.failure.store(4, Ordering::Release);
            }
            QUEUED_ARBITRATION.outcomes.fetch_or(1, Ordering::AcqRel);
        }
        Ok(_) => QUEUED_ARBITRATION.failure.store(1, Ordering::Release),
        Err(code) => QUEUED_ARBITRATION.failure.store(code, Ordering::Release),
    }
    match park_for_test() {
        Ok(WaitOutcome::Cancelled) => {
            QUEUED_ARBITRATION.outcomes.fetch_or(2, Ordering::AcqRel);
        }
        Ok(_) => QUEUED_ARBITRATION.failure.store(2, Ordering::Release),
        Err(code) => QUEUED_ARBITRATION.failure.store(code, Ordering::Release),
    }
    if QUEUED_ARBITRATION.done.release().is_err() {
        QUEUED_ARBITRATION.failure.store(3, Ordering::Release);
    }
}

fn park_for_test() -> Result<WaitOutcome, usize> {
    scheduler::ensure_sleepable().map_err(|_| 10usize)?;
    // SAFETY: The retained local mask is either consumed by the committed park
    // transition or dropped on the completed-without-parking path.
    let (prepared, interrupt_mask) = unsafe {
        QUEUED_ARBITRATION.state.with_mask_retained(|state| {
            let registration =
                scheduler::begin_wait(WaitMobility::Migratable).map_err(|_| 11usize)?;
            state.published_ticket = Some(registration.ticket());
            scheduler::prepare_registered_park_locked(&state.queue, registration)
                .map_err(|_| 12usize)
        })
    };
    match prepared? {
        scheduler::PrepareWait::Park(commit) => Ok(scheduler::complete_park(
            scheduler::retain_park_mask(commit, interrupt_mask),
        )),
        scheduler::PrepareWait::Completed(outcome) => {
            drop(interrupt_mask);
            Ok(outcome)
        }
    }
}

/// Proves that a CPU-local registration blocks reassignment while Armed.
///
/// The test is conditional only on a second admitted CPU being available;
/// single-CPU systems still execute every arbitration case above through the
/// same scheduler implementation.
fn exercise_cpu_local_migration_barrier() -> Result<(), Error> {
    if crate::kernel::cpu::online_cpu_count() < 2 {
        return Ok(());
    }
    CPU_LOCAL_FAILURE.store(0, Ordering::Release);
    let target = CpuIndex::new(1).ok_or(Error::StateMismatch(12))?;
    let affinity = CpuMask::EMPTY.with_cpu(CpuIndex::BOOT).with_cpu(target);
    let worker = scheduler::kthread_create_with_affinity(
        "wait/cpu-local-armed",
        cpu_local_worker,
        0,
        affinity,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    wait_for_completion(&CPU_LOCAL_DONE, 24)?;
    let failure = CPU_LOCAL_FAILURE.load(Ordering::Acquire);
    if failure != 0 {
        return Err(Error::StateMismatch(100 + failure));
    }
    Ok(())
}

/// Waits for a worker without entering an unbounded completion wait.
///
/// The timed observer blocks briefly between nonblocking claims, allowing a
/// remote worker to progress while retaining a deterministic failure bound.
fn wait_for_completion(done: &Semaphore, failure: usize) -> Result<(), Error> {
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(done.try_acquire()),
    )? {
        Ok(())
    } else {
        Err(Error::StateMismatch(failure))
    }
}

extern "C" fn cpu_local_worker(_argument: usize) {
    let result = (|| {
        let current_cpu = crate::kernel::cpu::current_index().ok_or(1usize)?;
        let target = if current_cpu == CpuIndex::BOOT {
            CpuIndex::new(1).ok_or(2usize)?
        } else {
            CpuIndex::BOOT
        };
        let current = scheduler::current_thread_id().map_err(|_| 3usize)?;
        let registration = scheduler::begin_wait(WaitMobility::CpuLocal).map_err(|_| 4usize)?;
        let ticket = registration.ticket();
        if scheduler::migrate_thread(current, target)
            != Err(scheduler::Error::MigrationBlockedByCpuLocalWait)
        {
            return Err(5);
        }
        let winner = scheduler::resolve_wait(ticket, WaitOutcome::Cancelled).map_err(|_| 6usize)?;
        let loser = scheduler::resolve_wait(ticket, WaitOutcome::TimedOut).map_err(|_| 7usize)?;
        if !winner.won || winner.made_ready || loser.won || loser.made_ready {
            return Err(8);
        }
        if scheduler::finish_wait(registration).map_err(|_| 9usize)? != Some(WaitOutcome::Cancelled)
        {
            return Err(10);
        }
        Ok(())
    })();
    if let Err(code) = result {
        CPU_LOCAL_FAILURE.store(code, Ordering::Release);
    }
    if CPU_LOCAL_DONE.release().is_err() {
        CPU_LOCAL_FAILURE.store(11, Ordering::Release);
    }
}
