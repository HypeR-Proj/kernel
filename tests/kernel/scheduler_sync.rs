// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real-context scheduler, wait-queue, Mutex, and Semaphore tests.
//!
//! The worker-backed cases use static state because the boot self-test suite is
//! intentionally one-shot and a completion semaphore is not a thread join.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::kernel::sync::{Completion, Mutex, Semaphore};
use crate::kernel::task::WaitQueue;
use crate::kernel::task::scheduler::{self, CpuMask, ThreadPriority};

static FIFO_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static FIFO_FAILURE: AtomicUsize = AtomicUsize::new(0);
static POLICY_DONE: Semaphore = Semaphore::new(0);

struct FairRotationState {
    ran: AtomicUsize,
    done: Semaphore,
}

static FAIR_ROTATION: FairRotationState = FairRotationState {
    ran: AtomicUsize::new(0),
    done: Semaphore::new(0),
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Affinity(usize),
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    Synchronization(crate::kernel::sync::Error),
    Worker(usize),
    StateMismatch,
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

struct SyncState {
    value: Mutex<usize>,
    gate: Semaphore,
    done: Semaphore,
    next_order: AtomicUsize,
    consumer_order: [AtomicUsize; 2],
    worker_error: AtomicUsize,
}

impl SyncState {
    const fn new() -> Self {
        Self {
            value: Mutex::new(0),
            gate: Semaphore::new(0),
            done: Semaphore::new(0),
            next_order: AtomicUsize::new(1),
            consumer_order: [AtomicUsize::new(0), AtomicUsize::new(0)],
            worker_error: AtomicUsize::new(0),
        }
    }

    fn fail(&self, code: usize) {
        let _ = self
            .worker_error
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
    }
}

static SYNC_STATE: SyncState = SyncState::new();

struct WaitState {
    queue: WaitQueue,
    done: Semaphore,
    order: AtomicUsize,
    observed: [AtomicUsize; 2],
    worker_error: AtomicUsize,
}

impl WaitState {
    const fn new() -> Self {
        Self {
            queue: WaitQueue::new(),
            done: Semaphore::new(0),
            order: AtomicUsize::new(1),
            observed: [AtomicUsize::new(0), AtomicUsize::new(0)],
            worker_error: AtomicUsize::new(0),
        }
    }
}

static WAIT_STATE: WaitState = WaitState::new();

struct CompletionState {
    event: Completion,
    done: Semaphore,
    observed: AtomicUsize,
    worker_error: AtomicUsize,
}

impl CompletionState {
    const fn new() -> Self {
        Self {
            event: Completion::new(),
            done: Semaphore::new(0),
            observed: AtomicUsize::new(0),
            worker_error: AtomicUsize::new(0),
        }
    }
}

static COMPLETION_STATE: CompletionState = CompletionState::new();

pub(super) fn run() -> Result<(), Error> {
    exercise_nonblocking_paths()?;
    exercise_affinity_creation()?;
    exercise_fair_rotation()?;
    exercise_policy_transitions()?;
    exercise_fifo_preemption_points()?;
    exercise_mutex_and_semaphore_handoff()?;
    exercise_completion_events()?;
    exercise_wait_queue_wake_all()?;
    let stats = super::support::quiesce_workers()?;
    if stats.real_time_class_threads + stats.fair_class_threads + stats.idle_class_threads
        != stats.threads
        || stats.idle_class_threads != stats.idle
    {
        return Err(Error::StateMismatch);
    }
    Ok(())
}

fn wait_for_completions(semaphore: &Semaphore, mut remaining: usize) -> Result<(), Error> {
    let completed = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || {
            while remaining != 0 && semaphore.try_acquire() {
                remaining -= 1;
            }
            Ok::<_, Error>(remaining == 0)
        },
    )?;
    if completed {
        Ok(())
    } else {
        Err(Error::StateMismatch)
    }
}

fn exercise_policy_transitions() -> Result<(), Error> {
    FIFO_SEQUENCE.store(0, Ordering::Release);

    let guard = scheduler::preempt_disable()?;
    let dormant =
        scheduler::kthread_create_fifo("policy-dormant", policy_peer, 1, ThreadPriority::HIGHEST)?;
    scheduler::set_thread_fair_policy(dormant)?;
    scheduler::thread_ready(dormant)?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch);
    }
    drop(guard);
    let _ = scheduler::cond_resched()?;
    wait_for_completions(&POLICY_DONE, 1)?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 1 {
        return Err(Error::StateMismatch);
    }
    let _ = super::support::quiesce_workers()?;

    let guard = scheduler::preempt_disable()?;
    let ready = scheduler::kthread_create("policy-ready", policy_peer, 2)?;
    scheduler::thread_ready(ready)?;
    scheduler::set_thread_fifo_policy(ready, ThreadPriority::HIGHEST)?;
    scheduler::set_thread_fair_policy(ready)?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 1 {
        return Err(Error::StateMismatch);
    }
    drop(guard);
    let _ = scheduler::cond_resched()?;
    wait_for_completions(&POLICY_DONE, 1)?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 2 {
        return Err(Error::StateMismatch);
    }
    let _ = super::support::quiesce_workers()?;

    let current = scheduler::current_thread_id()?;
    scheduler::set_thread_fifo_policy(current, ThreadPriority::HIGHEST)?;
    let peer =
        scheduler::kthread_create_fifo("policy-running", policy_peer, 3, ThreadPriority::NORMAL)?;
    scheduler::thread_ready(peer)?;
    let switches = scheduler::statistics()?.context_switches;
    scheduler::set_thread_fair_policy(current)?;
    let _ = scheduler::cond_resched()?;
    if scheduler::statistics()?.context_switches == switches {
        return Err(Error::StateMismatch);
    }
    wait_for_completions(&POLICY_DONE, 1)?;
    let _ = super::support::quiesce_workers()?;

    if FIFO_SEQUENCE.load(Ordering::Acquire) != 3 {
        return Err(Error::StateMismatch);
    }

    let guard = scheduler::preempt_disable()?;
    let candidate = scheduler::kthread_create_fifo(
        "policy-fair-to-fifo",
        policy_peer,
        4,
        ThreadPriority::NORMAL,
    )?;
    scheduler::thread_ready(candidate)?;
    if !scheduler::set_thread_fifo_policy_for_test(current, ThreadPriority::LOWEST)?
        || FIFO_SEQUENCE.load(Ordering::Acquire) != 3
    {
        return Err(Error::StateMismatch);
    }
    let _ = scheduler::preempt_enable_and_reschedule(guard)?;
    wait_for_completions(&POLICY_DONE, 1)?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 4 {
        return Err(Error::StateMismatch);
    }
    let _ = super::support::quiesce_workers()?;
    scheduler::set_thread_fair_policy(current)?;
    Ok(())
}

fn exercise_fair_rotation() -> Result<(), Error> {
    FAIR_ROTATION.ran.store(0, Ordering::Release);
    let _ = scheduler::cond_resched()?;
    let guard = scheduler::preempt_disable()?;
    let first = scheduler::kthread_create("fair-peer/0", fair_rotation_peer, 1 << 0)?;
    let second = scheduler::kthread_create("fair-peer/1", fair_rotation_peer, 1 << 1)?;
    scheduler::thread_ready(first)?;
    scheduler::thread_ready(second)?;

    // A Fair wakeup does not immediately displace an equal-class thread. One
    // deliberately oversized charge expires the private backend quantum while
    // preemption remains deferred; releasing the guard must let both peers
    // run. Completion order is deliberately not asserted because a timer may
    // preempt either Fair worker after dispatch.
    if FAIR_ROTATION.ran.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch);
    }
    let switches = scheduler::statistics()?.context_switches;
    scheduler::account_tick(u64::MAX)?;
    let _ = scheduler::preempt_enable_and_reschedule(guard)?;
    if scheduler::statistics()?.context_switches == switches {
        return Err(Error::StateMismatch);
    }
    wait_for_completions(&FAIR_ROTATION.done, 2)?;
    if FAIR_ROTATION.ran.load(Ordering::Acquire) != 0b11 {
        return Err(Error::StateMismatch);
    }
    let _ = super::support::quiesce_workers()?;
    Ok(())
}

extern "C" fn fair_rotation_peer(bit: usize) {
    FAIR_ROTATION.ran.fetch_or(bit, Ordering::AcqRel);
    let _ = FAIR_ROTATION.done.release();
}

extern "C" fn policy_peer(expected: usize) {
    fifo_peer(expected);
    let _ = POLICY_DONE.release();
}

fn exercise_affinity_creation() -> Result<(), Error> {
    let current = crate::kernel::cpu::current_index().ok_or(Error::Affinity(1))?;
    if current != CpuIndex::BOOT {
        return Err(Error::Affinity(2));
    }
    let online_cpus = crate::kernel::cpu::online_cpu_count();
    let remote = if online_cpus > 1 {
        CpuIndex::new(1)
    } else {
        None
    };
    let local_affinity = remote.map_or(CpuMask::single(current), |remote| {
        CpuMask::EMPTY.with_cpu(current).with_cpu(remote)
    });
    let local = scheduler::kthread_create_with_affinity(
        "affinity-local",
        affinity_local_worker,
        0,
        local_affinity,
    )?;
    if scheduler::thread_placement(local)? != (current, local_affinity) {
        return Err(Error::Affinity(4));
    }
    scheduler::thread_ready(local)?;
    scheduler::yield_now()?;

    if let Some(remote) = remote {
        let remote_only = CpuMask::single(remote);
        let remote_thread = scheduler::kthread_create_with_affinity(
            "affinity-remote",
            affinity_local_worker,
            0,
            remote_only,
        )?;
        if scheduler::thread_placement(remote_thread)? != (remote, remote_only) {
            return Err(Error::Affinity(5));
        }
        scheduler::discard_dormant_kernel_thread(remote_thread)?;
    }

    if !matches!(
        scheduler::kthread_create_with_affinity("affinity-empty", fifo_peer, 0, CpuMask::EMPTY),
        Err(scheduler::Error::EmptyCpuAffinity)
    ) {
        return Err(Error::Affinity(7));
    }

    if let Some(unregistered) = CpuIndex::new(online_cpus)
        && !matches!(
            scheduler::kthread_create_with_affinity(
                "affinity-offline",
                fifo_peer,
                0,
                CpuMask::single(unregistered)
            ),
            Err(scheduler::Error::NoRegisteredCpuInAffinity)
        )
    {
        return Err(Error::Affinity(8));
    }

    let registry_before = scheduler::registry_slot_count()?;
    let before = scheduler::kthread_create("reservation-before", fifo_peer, 0)?;
    scheduler::discard_dormant_kernel_thread(before)?;
    if scheduler::kthread_create(
        "this-thread-name-is-deliberately-longer-than-the-fixed-capacity",
        fifo_peer,
        0,
    )
    .is_ok()
    {
        return Err(Error::Affinity(9));
    }
    let after = scheduler::kthread_create("reservation-after", fifo_peer, 0)?;
    if after.get() <= before.get() + 1 {
        return Err(Error::Affinity(10));
    }
    if !matches!(
        scheduler::thread_placement(before),
        Err(scheduler::Error::ThreadNotFound)
    ) || scheduler::registry_slot_count()? > registry_before.saturating_add(1)
    {
        return Err(Error::Affinity(11));
    }
    scheduler::discard_dormant_kernel_thread(after)?;
    let stable_slots = scheduler::registry_slot_count()?;
    for _ in 0..64 {
        let id = scheduler::kthread_create("registry-reuse", fifo_peer, 0)?;
        scheduler::discard_dormant_kernel_thread(id)?;
    }
    if scheduler::registry_slot_count()? != stable_slots {
        return Err(Error::Affinity(12));
    }
    Ok(())
}

extern "C" fn affinity_local_worker(_argument: usize) {}

fn exercise_fifo_preemption_points() -> Result<(), Error> {
    FIFO_SEQUENCE.store(0, Ordering::Release);

    // The bootstrap Thread is Fair by default. Move it explicitly into RT FIFO
    // so this test isolates equal-priority FIFO head preservation.
    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::NORMAL)?;

    let guard = scheduler::preempt_disable()?;
    let nested = scheduler::preempt_disable()?;
    if !matches!(
        scheduler::yield_now(),
        Err(scheduler::Error::CannotSleepWithPreemptionDisabled)
    ) || scheduler::preempt_enable_and_reschedule(nested)?
    {
        return Err(Error::StateMismatch);
    }
    if scheduler::preempt_enable_and_reschedule(guard)? {
        return Err(Error::StateMismatch);
    }

    let first =
        scheduler::kthread_create_fifo("fifo-peer/0", fifo_peer, 2, ThreadPriority::NORMAL)?;
    let second =
        scheduler::kthread_create_fifo("fifo-peer/1", fifo_peer, 3, ThreadPriority::NORMAL)?;
    let higher =
        scheduler::kthread_create_fifo("fifo-higher", fifo_peer, 1, ThreadPriority::HIGHEST)?;
    scheduler::thread_ready(first)?;
    scheduler::thread_ready(second)?;
    scheduler::set_thread_fifo_policy(first, ThreadPriority::NORMAL)?;
    scheduler::thread_ready(higher)?;

    let _ = scheduler::cond_resched()?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 1 {
        return Err(Error::StateMismatch);
    }
    scheduler::yield_now()?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != 3 {
        return Err(Error::StateMismatch);
    }
    scheduler::yield_now()?;

    scheduler::set_thread_fifo_policy(scheduler::current_thread_id()?, ThreadPriority::LOWEST)?;
    exercise_running_priority_changes(0, 3)?;
    exercise_running_priority_changes(1, 2)?;
    exercise_running_priority_changes(2, 3)?;
    scheduler::set_thread_fair_policy(scheduler::current_thread_id()?)?;
    Ok(())
}

fn exercise_running_priority_changes(mode: usize, expected: usize) -> Result<(), Error> {
    FIFO_SEQUENCE.store(0, Ordering::Release);
    FIFO_FAILURE.store(0, Ordering::Release);
    let worker = scheduler::kthread_create_fifo(
        "fifo-priority-change",
        fifo_priority_change,
        mode,
        ThreadPriority::NORMAL,
    )?;
    scheduler::thread_ready(worker)?;
    scheduler::yield_now()?;
    if FIFO_SEQUENCE.load(Ordering::Acquire) != expected
        || FIFO_FAILURE.load(Ordering::Acquire) != 0
    {
        return Err(Error::StateMismatch);
    }
    Ok(())
}

extern "C" fn fifo_priority_change(mode: usize) {
    let current = match scheduler::current_thread_id() {
        Ok(id) => id,
        Err(_) => return record_fifo_failure(1),
    };
    let equal_expected = if mode == 2 { 3 } else { 1 };
    let equal = match scheduler::kthread_create_fifo(
        "fifo-priority-peer",
        fifo_peer,
        equal_expected,
        ThreadPriority::new(64),
    ) {
        Ok(id) => id,
        Err(_) => return record_fifo_failure(2),
    };
    let guard = match scheduler::preempt_disable() {
        Ok(guard) => guard,
        Err(_) => return record_fifo_failure(3),
    };
    if scheduler::thread_ready(equal).is_err() {
        return record_fifo_failure(4);
    }

    if mode == 0 {
        let lower = match scheduler::kthread_create_fifo(
            "fifo-lower-peer",
            fifo_peer,
            3,
            ThreadPriority::new(200),
        ) {
            Ok(id) => id,
            Err(_) => return record_fifo_failure(5),
        };
        if scheduler::thread_ready(lower).is_err()
            || scheduler::set_thread_fifo_policy(current, ThreadPriority::new(64)).is_err()
            || scheduler::set_thread_fifo_policy(current, ThreadPriority::new(200)).is_err()
            || scheduler::preempt_enable_and_reschedule(guard).is_err()
        {
            return record_fifo_failure(6);
        }
    } else if mode == 1 {
        if scheduler::set_thread_fifo_policy(current, ThreadPriority::new(200)).is_err()
            || scheduler::set_thread_fifo_policy(current, ThreadPriority::new(64)).is_err()
            || scheduler::preempt_enable_and_reschedule(guard).is_err()
        {
            return record_fifo_failure(7);
        }
    } else {
        if scheduler::set_thread_fifo_policy(current, ThreadPriority::new(64)).is_err()
            || scheduler::set_thread_fifo_policy(equal, ThreadPriority::new(200)).is_err()
            || !matches!(scheduler::preempt_enable_and_reschedule(guard), Ok(false))
        {
            return record_fifo_failure(8);
        }
        let future_equal = match scheduler::kthread_create_fifo(
            "fifo-future-peer",
            fifo_peer,
            1,
            ThreadPriority::new(64),
        ) {
            Ok(id) => id,
            Err(_) => return record_fifo_failure(9),
        };
        if scheduler::thread_ready(future_equal).is_err()
            || !matches!(scheduler::cond_resched(), Ok(false))
            || scheduler::yield_now().is_err()
        {
            return record_fifo_failure(10);
        }
    }
    fifo_peer(2);
}

fn record_fifo_failure(code: usize) {
    FIFO_FAILURE.store(code, Ordering::Release);
}

extern "C" fn fifo_peer(expected: usize) {
    let _ =
        FIFO_SEQUENCE.compare_exchange(expected - 1, expected, Ordering::AcqRel, Ordering::Acquire);
}

fn exercise_nonblocking_paths() -> Result<(), Error> {
    let semaphore = Semaphore::new(2);
    if semaphore.available_permits() != 2
        || !semaphore.try_acquire()
        || !semaphore.try_acquire()
        || semaphore.try_acquire()
    {
        return Err(Error::StateMismatch);
    }
    semaphore.release()?;
    if semaphore.available_permits() != 1 {
        return Err(Error::StateMismatch);
    }
    let mutex = Mutex::new(7usize);
    let guard = mutex.try_lock()?.ok_or(Error::StateMismatch)?;
    if *guard != 7 || mutex.waiter_count()? != 0 {
        return Err(Error::StateMismatch);
    }
    if !matches!(
        mutex.try_lock(),
        Err(crate::kernel::sync::Error::WouldDeadlock)
    ) {
        return Err(Error::StateMismatch);
    }
    drop(guard);

    let completion = Completion::new();
    if completion.is_complete() || completion.try_wait() {
        return Err(Error::StateMismatch);
    }
    completion.complete()?;
    if !completion.is_complete() || !completion.try_wait() || completion.try_wait() {
        return Err(Error::StateMismatch);
    }
    completion.complete_all()?;
    if !completion.try_wait() || !completion.try_wait() {
        return Err(Error::StateMismatch);
    }
    Ok(())
}

fn exercise_completion_events() -> Result<(), Error> {
    let state = &COMPLETION_STATE;
    let guard = scheduler::preempt_disable()?;
    for index in 0..2 {
        let name = if index == 0 {
            "completion/0"
        } else {
            "completion/1"
        };
        let waiter =
            scheduler::kthread_create_fifo(name, completion_waiter, index, ThreadPriority::NORMAL)?;
        scheduler::thread_ready(waiter)?;
    }
    let _ = scheduler::preempt_enable_and_reschedule(guard)?;
    if state.event.waiter_count()? != 2 {
        return Err(Error::StateMismatch);
    }

    state.event.complete()?;
    if state.event.waiter_count()? != 1 {
        return Err(Error::StateMismatch);
    }
    state.event.complete_all()?;
    scheduler::yield_now()?;
    wait_for_completions(&state.done, 2)?;
    if state.observed.load(Ordering::Acquire) != 0b11
        || state.worker_error.load(Ordering::Acquire) != 0
        || !state.event.is_complete()
    {
        return Err(Error::StateMismatch);
    }
    Ok(())
}

extern "C" fn completion_waiter(argument: usize) {
    let state = &COMPLETION_STATE;
    if state.event.wait().is_err() {
        state.worker_error.store(1, Ordering::Release);
    } else if let Some(bit) = [0b01usize, 0b10].get(argument).copied() {
        state.observed.fetch_or(bit, Ordering::AcqRel);
    } else {
        state.worker_error.store(2, Ordering::Release);
    }
    if state.done.release().is_err() {
        state.worker_error.store(3, Ordering::Release);
    }
}

fn exercise_mutex_and_semaphore_handoff() -> Result<(), Error> {
    let state = &SYNC_STATE;
    let producer = scheduler::kthread_create("sync-producer", producer, 0)?;
    let first = scheduler::kthread_create("sync-consumer/0", consumer, 0)?;
    let second = scheduler::kthread_create("sync-consumer/1", consumer, 1)?;
    let guard = scheduler::preempt_disable()?;
    scheduler::thread_ready(producer)?;
    if !scheduler::thread_ready(first)? || scheduler::thread_ready(first)? {
        return Err(Error::StateMismatch);
    }
    scheduler::thread_ready(second)?;
    scheduler::set_thread_fifo_policy(first, ThreadPriority::HIGHEST)?;
    scheduler::set_thread_fifo_policy(second, ThreadPriority::HIGHEST)?;
    drop(guard);
    scheduler::yield_now()?;
    for _ in 0..3 {
        state.done.acquire()?;
    }
    scheduler::yield_now()?;
    verify_sync_state(state)
}

fn verify_sync_state(state: &SyncState) -> Result<(), Error> {
    let error = state.worker_error.load(Ordering::Acquire);
    let value = *state.value.lock()?;
    let order = [
        state.consumer_order[0].load(Ordering::Acquire),
        state.consumer_order[1].load(Ordering::Acquire),
    ];
    if error == 0 && value == 3 && order == [1, 2] {
        Ok(())
    } else if error != 0 {
        Err(Error::Worker(error))
    } else {
        Err(Error::StateMismatch)
    }
}

extern "C" fn producer(_argument: usize) {
    let state = &SYNC_STATE;
    let Ok(mut value) = state.value.lock() else {
        state.fail(1);
        let _ = state.done.release();
        return;
    };
    *value = 1;
    if !matches!(
        state.value.try_lock(),
        Err(crate::kernel::sync::Error::WouldDeadlock)
    ) {
        state.fail(2);
    }
    if state.gate.release().is_err() || state.gate.release().is_err() {
        state.fail(3);
    }
    if scheduler::yield_now().is_err() || state.value.waiter_count() != Ok(2) {
        state.fail(4);
    }
    drop(value);
    if state.done.release().is_err() {
        state.fail(5);
    }
}

extern "C" fn consumer(argument: usize) {
    let state = &SYNC_STATE;
    let Some(order_slot) = state.consumer_order.get(argument) else {
        state.fail(6);
        let _ = state.done.release();
        return;
    };
    if state.gate.acquire().is_err() {
        state.fail(7 + argument);
    } else {
        if !matches!(state.value.try_lock(), Ok(None)) {
            state.fail(9 + argument);
        }
        match state.value.lock() {
            Ok(mut value) => {
                *value += 1;
                let order = state.next_order.fetch_add(1, Ordering::AcqRel);
                order_slot.store(order, Ordering::Release);
            }
            Err(_) => state.fail(11 + argument),
        }
    }
    if state.done.release().is_err() {
        state.fail(13 + argument);
    }
}

fn exercise_wait_queue_wake_all() -> Result<(), Error> {
    let state = &WAIT_STATE;
    let guard = scheduler::preempt_disable()?;
    for index in 0..2 {
        let id = scheduler::kthread_create_fifo(
            if index == 0 { "waiter/0" } else { "waiter/1" },
            waiter,
            index,
            ThreadPriority::NORMAL,
        )?;
        scheduler::thread_ready(id)?;
    }
    let _ = scheduler::preempt_enable_and_reschedule(guard)?;
    if state.queue.len()? != 2 || state.queue.wake_all()? != 2 {
        return Err(Error::StateMismatch);
    }
    scheduler::yield_now()?;
    wait_for_completions(&state.done, 2)?;
    scheduler::yield_now()?;
    let observed = [
        state.observed[0].load(Ordering::Acquire),
        state.observed[1].load(Ordering::Acquire),
    ];
    if observed != [1, 2] || state.worker_error.load(Ordering::Acquire) != 0 {
        return Err(Error::StateMismatch);
    }
    Ok(())
}

extern "C" fn waiter(argument: usize) {
    let state = &WAIT_STATE;
    let Some(observed) = state.observed.get(argument) else {
        state.worker_error.store(3, Ordering::Release);
        let _ = state.done.release();
        return;
    };
    if state.queue.wait().is_err() {
        state.worker_error.store(1, Ordering::Release);
    } else {
        let order = state.order.fetch_add(1, Ordering::AcqRel);
        observed.store(order, Ordering::Release);
    }
    if state.done.release().is_err() {
        state.worker_error.store(2, Ordering::Release);
    }
}
