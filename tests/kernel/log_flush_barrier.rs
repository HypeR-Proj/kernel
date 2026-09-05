// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-integrated cancellation and reuse tests for log flush barriers.

use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::kernel::log::console;
use crate::kernel::sync::Semaphore;
use crate::kernel::task::WaitOutcome;
use crate::kernel::task::scheduler::{self, ThreadPriority};
use crate::kernel::task::thread::ThreadId;

const EMPTY_SLOT: usize = usize::MAX;
const MAX_PROGRESS_PASSES: usize = 4_096;

static SLOTS: [AtomicUsize; 2] = [AtomicUsize::new(EMPTY_SLOT), AtomicUsize::new(EMPTY_SLOT)];
static OUTCOMES: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static DONE: Semaphore = Semaphore::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Quiescence(super::support::QuiescenceError),
    Scheduler(scheduler::Error),
    State(usize),
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    reset_worker_state(2);
    let first = create_workers(2)?;
    let first_slots = wait_until_queued(2)?;
    if first_slots[0] == first_slots[1] {
        return Err(Error::State(1));
    }
    cancel_workers(&first, &first_slots, 2)?;
    wait_until_done(2)?;
    verify_retired(2)?;

    reset_worker_state(1);
    let reused = create_workers(1)?;
    let reused_slots = wait_until_queued(1)?;
    if reused_slots[0] != first_slots[0] && reused_slots[0] != first_slots[1] {
        return Err(Error::State(2));
    }
    cancel_workers(&reused, &reused_slots, 1)?;
    wait_until_done(1)?;
    verify_retired(1)?;

    let _ = super::support::quiesce_workers()?;
    Ok(())
}

fn reset_worker_state(count: usize) {
    for index in 0..count {
        SLOTS[index].store(EMPTY_SLOT, Ordering::Release);
        OUTCOMES[index].store(0, Ordering::Release);
    }
}

fn create_workers(count: usize) -> Result<[Option<ThreadId>; 2], Error> {
    let guard = scheduler::preempt_disable()?;
    let mut workers = [None; 2];
    for (index, worker) in workers.iter_mut().enumerate().take(count) {
        let thread = scheduler::kthread_create_fifo(
            "log-flush/cancel",
            flush_waiter,
            index,
            ThreadPriority::NORMAL,
        )?;
        if !scheduler::thread_ready(thread)? {
            return Err(Error::State(3));
        }
        *worker = Some(thread);
    }
    let _ = scheduler::preempt_enable_and_reschedule(guard)?;
    Ok(workers)
}

fn wait_until_queued(count: usize) -> Result<[usize; 2], Error> {
    for _ in 0..MAX_PROGRESS_PASSES {
        let mut slots = [EMPTY_SLOT; 2];
        let mut queued = true;
        for (index, slot) in slots.iter_mut().enumerate().take(count) {
            *slot = SLOTS[index].load(Ordering::Acquire);
            if *slot == EMPTY_SLOT || console::flush_barrier_waiter_count_for_test(*slot)? != 1 {
                queued = false;
            }
        }
        if queued {
            return Ok(slots);
        }
        scheduler::yield_now()?;
    }
    Err(Error::State(4))
}

fn cancel_workers(
    workers: &[Option<ThreadId>; 2],
    slots: &[usize; 2],
    count: usize,
) -> Result<(), Error> {
    for index in 0..count {
        let Some(worker) = workers[index] else {
            return Err(Error::State(5));
        };
        if !console::cancel_flush_barrier_waiter_for_test(slots[index], worker)? {
            return Err(Error::State(6));
        }
    }
    Ok(())
}

fn wait_until_done(mut remaining: usize) -> Result<(), Error> {
    for _ in 0..MAX_PROGRESS_PASSES {
        while remaining != 0 && DONE.try_acquire() {
            remaining -= 1;
        }
        if remaining == 0 {
            return Ok(());
        }
        scheduler::yield_now()?;
    }
    Err(Error::State(7))
}

fn verify_retired(count: usize) -> Result<(), Error> {
    for outcome in OUTCOMES.iter().take(count) {
        if outcome.load(Ordering::Acquire) != 1 {
            return Err(Error::State(8));
        }
    }
    if console::active_flush_barrier_count_for_test() != 0 {
        return Err(Error::State(9));
    }
    Ok(())
}

extern "C" fn flush_waiter(index: usize) {
    let outcome = match console::register_pending_flush_barrier_for_test() {
        Ok(barrier) => {
            SLOTS[index].store(barrier.slot_for_test(), Ordering::Release);
            match console::wait_pending_flush_barrier_for_test(barrier) {
                Err(crate::kernel::sync::Error::WaitInterrupted(WaitOutcome::Cancelled)) => 1,
                Ok(_) => 2,
                Err(_) => 3,
            }
        }
        Err(_) => 4,
    };
    OUTCOMES[index].store(outcome, Ordering::Release);
    if DONE.release().is_err() {
        OUTCOMES[index].store(5, Ordering::Release);
    }
}
