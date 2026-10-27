// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Executed scheduler arbitration and authority-close contracts for file locks.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU8, Ordering};

use super::{FileLocks, LockError, LockMode, LockOwner};
use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {crate::task, crate::task::WaitOutcome, crate::task::scheduler};

#[derive(Debug)]
pub(crate) enum Error {
    Construction,
    Lock(LockError),
    Scheduler(scheduler::Error),
    Progress,
    Quiescence,
    State(u32),
}

impl From<LockError> for Error {
    fn from(error: LockError) -> Self {
        Self::Lock(error)
    }
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<task::SleepError> for Error {
    fn from(_: task::SleepError) -> Self {
        Self::Progress
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    NotifyWins,
    TimeoutWins,
    CancelWins,
    CloseWaiter,
    CloseHolder,
    UnlockWaiter,
    TimerExpires,
}

struct Context {
    locks: FileLocks,
    holder: LockOwner,
    waiter: LockOwner,
    observer: LockOwner,
    domain: ResourceDomain,
    deadline: u64,
    completed: AtomicU8,
}

pub(crate) fn run(quiesce: impl Fn() -> bool) -> Result<(), Error> {
    for scenario in [
        Scenario::NotifyWins,
        Scenario::TimeoutWins,
        Scenario::CancelWins,
        Scenario::CloseWaiter,
        Scenario::CloseHolder,
        Scenario::UnlockWaiter,
        Scenario::TimerExpires,
    ] {
        exercise(scenario, &quiesce)?;
    }
    Ok(())
}

fn exercise(scenario: Scenario, quiesce: &impl Fn() -> bool) -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let locks = FileLocks::new();
    let holder = locks
        .create_owner(&domain)
        .map_err(|_| Error::Construction)?;
    let waiter = locks
        .create_owner(&domain)
        .map_err(|_| Error::Construction)?;
    let observer = locks
        .create_owner(&domain)
        .map_err(|_| Error::Construction)?;
    let deadline = if matches!(scenario, Scenario::TimerExpires) {
        crate::time::monotonic_nanoseconds()
            .ok()
            .and_then(|now| now.checked_add(200_000_000))
            .ok_or(Error::Construction)?
    } else {
        hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE
    };
    let context = hyper::mm::try_box(Context {
        locks,
        holder,
        waiter,
        observer,
        domain,
        deadline,
        completed: AtomicU8::new(0),
    })
    .map_err(|_| Error::Construction)?;
    // A failure to prove worker quiescence deliberately retains the context:
    // the harness fail-stops, and no worker may observe prematurely freed data.
    let context = Box::leak(context);
    let result = exercise_owned(context, scenario);
    context.locks.close(&context.holder);
    context.locks.close(&context.waiter);
    context.locks.close(&context.observer);
    if !quiesce() {
        return Err(Error::Quiescence);
    }
    // SAFETY: this is the original leaked Box allocation. Scheduler quiescence
    // proves all workers and exit trampolines finished; no pointer escapes run.
    drop(unsafe { Box::from_raw(core::ptr::from_mut(context)) });
    result
}

fn exercise_owned(context: &Context, scenario: Scenario) -> Result<(), Error> {
    context.locks.lock(
        &context.holder,
        LockMode::Exclusive,
        0,
        &context.domain,
        || false,
    )?;
    if !matches!(
        context.locks.lock(
            &context.waiter,
            LockMode::Shared,
            0,
            &context.domain,
            || false,
        ),
        Err(LockError::WouldBlock)
    ) {
        return Err(Error::State(1));
    }
    let worker = scheduler::kthread_create(
        "file-lock/waiter",
        wait,
        core::ptr::from_ref(context).expose_provenance(),
    )?;
    scheduler::thread_ready(worker)?;
    progress(|| {
        context.locks.state.with(|state| state.waiter_count == 1)
            || context.completed.load(Ordering::Acquire) != 0
    })?;
    let ticket = context
        .locks
        .state
        .with(|state| state.waiters.as_ref().map(|waiter| waiter.ticket))
        .ok_or(Error::State(2))?;
    match scenario {
        Scenario::NotifyWins => {
            // Keep the condition lock across both contenders so the timeout
            // cannot be confused with a subsequent scheduler wait generation.
            context.locks.state.with(|state| {
                let mut holder = context.holder.owner.load();
                state.grants.release(&mut holder);
                context.holder.owner.store(holder);
                state.reconcile();
                if scheduler::resolve_wait(ticket, WaitOutcome::TimedOut)?.won {
                    return Err(Error::State(3));
                }
                Ok(())
            })?;
        }
        Scenario::TimeoutWins | Scenario::CancelWins => {
            context.locks.state.with(|state| {
                let outcome = if matches!(scenario, Scenario::TimeoutWins) {
                    WaitOutcome::TimedOut
                } else {
                    WaitOutcome::Cancelled
                };
                if !scheduler::resolve_wait(ticket, outcome)?.won {
                    return Err(Error::State(4));
                }
                let mut holder = context.holder.owner.load();
                state.grants.release(&mut holder);
                context.holder.owner.store(holder);
                state.reconcile();
                Ok(())
            })?;
        }
        Scenario::CloseWaiter => {
            context.locks.close(&context.waiter);
            context.locks.unlock(&context.holder)?;
        }
        Scenario::CloseHolder => context.locks.close(&context.holder),
        Scenario::UnlockWaiter => {
            context.locks.unlock(&context.waiter)?;
            context.locks.unlock(&context.holder)?;
        }
        Scenario::TimerExpires => {}
    }
    progress(|| context.completed.load(Ordering::Acquire) != 0)?;
    let expected = match scenario {
        Scenario::NotifyWins | Scenario::CloseHolder => 1,
        Scenario::TimeoutWins | Scenario::TimerExpires => 2,
        Scenario::CancelWins | Scenario::CloseWaiter | Scenario::UnlockWaiter => 3,
    };
    if context.completed.load(Ordering::Acquire) != expected {
        return Err(Error::State(5));
    }
    if expected == 1
        && !matches!(
            context.locks.lock(
                &context.observer,
                LockMode::Exclusive,
                0,
                &context.domain,
                || false,
            ),
            Err(LockError::WouldBlock)
        )
    {
        return Err(Error::State(6));
    }
    context.locks.close(&context.holder);
    context.locks.close(&context.waiter);
    if !matches!(
        context.locks.lock(
            &context.waiter,
            LockMode::Exclusive,
            0,
            &context.domain,
            || false,
        ),
        Err(LockError::Closed)
    ) {
        return Err(Error::State(7));
    }
    context.locks.lock(
        &context.observer,
        LockMode::Exclusive,
        0,
        &context.domain,
        || false,
    )?;
    Ok(())
}

fn progress(condition: impl FnMut() -> bool) -> Result<(), Error> {
    let mut condition = condition;
    let reached = task::wait_for_test_progress(task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(condition())
    })
    .map_err(|_| Error::Progress)?;
    if reached {
        Ok(())
    } else {
        Err(Error::Progress)
    }
}

extern "C" fn wait(argument: usize) {
    // SAFETY: exercise retains the leaked boxed context until the shared
    // quiescence helper proves this worker and its exit trampoline retired.
    let context = unsafe { &*core::ptr::with_exposed_provenance::<Context>(argument) };
    let result = context.locks.lock(
        &context.waiter,
        LockMode::Exclusive,
        context.deadline,
        &context.domain,
        || false,
    );
    let result = match result {
        Ok(()) => 1,
        Err(LockError::TimedOut) => 2,
        Err(LockError::Cancelled) => 3,
        Err(LockError::Closed) => 4,
        Err(_) => 5,
    };
    context.completed.store(result, Ordering::Release);
}
