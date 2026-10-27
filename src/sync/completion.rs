// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Counted completion events for kernel thread synchronization.

use hyper::sync::InterruptSpinLock;

use super::Error;
use crate::task::scheduler;
use {crate::task::WaitMobility, crate::task::WaitQueue};

type StateLock = InterruptSpinLock<State, crate::hal::irq::LocalMask>;

struct State {
    done: Done,
    waiters: WaitQueue,
}

enum Done {
    Count(usize),
    All,
}

/// A counted completion event.
///
/// Each call to [`complete`](Self::complete) satisfies one waiter or retains one
/// completion for a future waiter. [`complete_all`](Self::complete_all)
/// satisfies every current and future waiter. This type deliberately has no
/// reset operation: resetting a completion while an older waiter is still
/// retiring would require an explicit generation protocol.
pub struct Completion {
    state: StateLock,
}

impl Completion {
    pub const fn new() -> Self {
        Self {
            state: StateLock::new(State {
                done: Done::Count(0),
                waiters: WaitQueue::new(),
            }),
        }
    }

    /// Waits until one completion is available and consumes it.
    pub fn wait(&self) -> Result<(), Error> {
        scheduler::ensure_sleepable()?;
        // SAFETY: The retained mask is consumed by the park transition or
        // dropped before this function resumes normal execution.
        let (park, interrupt_mask) = unsafe {
            self.state.with_mask_retained(|state| {
                if consume_one(state) {
                    Ok(None)
                } else {
                    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
                    scheduler::prepare_registered_park_locked(&state.waiters, registration)
                        .map(Some)
                        .map_err(Error::from)
                }
            })
        };
        let park = park?;
        let Some(prepared) = park else {
            drop(interrupt_mask);
            return Ok(());
        };
        let outcome = prepared.retain_mask(interrupt_mask).complete();
        super::expect_notification(outcome)
    }

    /// Joins kernel-owned work which must finish before the caller can retire.
    /// Cancellation wakes the caller but cannot release its lifetime obligation;
    /// rearm a fresh wait instead. Ordinary cancellable waits must use `wait`.
    pub(crate) fn wait_uninterruptible(&self) -> Result<(), Error> {
        loop {
            match self.wait() {
                Err(Error::WaitInterrupted(crate::task::WaitOutcome::Cancelled)) => {}
                result => return result,
            }
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn cancel_waiter_for_test(
        &self,
        thread: crate::task::thread::ThreadId,
    ) -> Result<bool, Error> {
        self.state
            .with(|state| state.waiters.cancel(thread).map_err(Into::into))
    }

    /// Consumes one retained completion without blocking.
    pub fn try_wait(&self) -> bool {
        self.state.with(consume_one)
    }

    /// Completes one waiter, retaining the event when no waiter is present.
    pub fn complete(&self) -> Result<(), Error> {
        self.state.with(|state| {
            if scheduler::wake_one(&state.waiters)?.is_none()
                && let Done::Count(count) = &mut state.done
            {
                *count = count.checked_add(1).ok_or(Error::CompletionOverflow)?;
            }
            Ok(())
        })
    }

    /// Completes all current and future waiters.
    pub fn complete_all(&self) -> Result<(), Error> {
        self.state.with(|state| {
            state.done = Done::All;
            scheduler::wake_all(&state.waiters)?;
            Ok(())
        })
    }

    pub fn is_complete(&self) -> bool {
        self.state
            .with(|state| !matches!(state.done, Done::Count(0)))
    }

    pub fn waiter_count(&self) -> Result<usize, Error> {
        self.state
            .with(|state| state.waiters.len().map_err(Error::from))
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

fn consume_one(state: &mut State) -> bool {
    match &mut state.done {
        Done::Count(0) => false,
        Done::Count(count) => {
            *count -= 1;
            true
        }
        Done::All => true,
    }
}
