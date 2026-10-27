// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deadline arbitration for scheduler wait queues.
//!
//! Timer setup and allocation happen before queue publication. The actual
//! park, notification, timeout, and cancellation transitions remain
//! allocation-free and are serialized by the scheduler's embedded wait
//! record. A timer stays on its source CPU even when its Thread migrates;
//! handle-directed cancellation retires it from that original queue.

use core::hint::spin_loop;

use alloc::boxed::Box;
use hyper::hal::timer::deadline_reached;
use hyper::sync::atomic::{AtomicBool, Ordering};

use super::scheduler;
use super::wait::{WaitMobility, WaitOutcome, WaitQueue, WaitTicket};

struct TimeoutContext {
    ticket: Option<WaitTicket>,
    callback_complete: AtomicBool,
    _source: Option<super::WaitSourceRegistration>,
}

impl TimeoutContext {
    const fn new() -> Self {
        Self {
            ticket: None,
            callback_complete: AtomicBool::new(false),
            _source: None,
        }
    }

    fn pointer(&self) -> usize {
        core::ptr::from_ref(self).expose_provenance()
    }

    fn wait_for_callback(&self) {
        while !self.callback_complete.load(Ordering::Acquire) {
            spin_loop();
        }
    }
}

/// Fallibly allocated callback storage which is not yet visible to a timer.
///
/// Condition-based waits prepare this value before taking their state lock or
/// publishing a scheduler registration. Arming consumes it, so a successful
/// return is the sole owner responsible for exact timer retirement.
pub(crate) struct PreparedTimeout {
    context: Box<TimeoutContext>,
}

impl PreparedTimeout {
    pub(crate) const fn allocation_size() -> usize {
        match core::mem::size_of::<TimeoutContext>()
            .checked_add(crate::time::queue::PendingTimer::allocation_size())
        {
            Some(bytes) => bytes,
            None => usize::MAX,
        }
    }

    pub(crate) fn try_new() -> Result<Self, TimedWaitError> {
        let context =
            hyper::mm::try_box(TimeoutContext::new()).map_err(|_| TimedWaitError::Allocation)?;
        Ok(Self { context })
    }

    /// Publishes an exact scheduler generation to a one-shot timer callback.
    ///
    /// `schedule_irq_at` either returns a live exact handle or proves that its
    /// provisional queue entry was detached before returning an error. The
    /// context may therefore be dropped normally on the error path.
    pub(crate) fn arm(
        mut self,
        ticket: WaitTicket,
        deadline: u64,
    ) -> Result<ArmedTimeout, TimedWaitError> {
        self.context.ticket = Some(ticket);
        self.context._source = Some(super::WaitSourceRegistration::new(
            ticket,
            super::WaitSource::Timer,
        ));
        let timer = crate::time::schedule_irq_at(
            deadline,
            crate::time::TimerMode::OneShot,
            expire_wait,
            self.context.pointer(),
        )?;
        Ok(ArmedTimeout::new(timer, self.context))
    }
}

/// Linear owner of one queued timer and its raw callback context.
///
/// The timer queue borrows `context` through an exposed pointer. This owner may
/// be destroyed only after exact-handle cancellation or callback completion has
/// ended that borrow.
#[must_use = "an armed timeout must be retired before its callback owner is dropped"]
pub(crate) struct ArmedTimeout {
    timer: Option<crate::time::TimerHandle>,
    context: Box<TimeoutContext>,
}

impl ArmedTimeout {
    const fn new(timer: crate::time::TimerHandle, context: Box<TimeoutContext>) -> Self {
        Self {
            timer: Some(timer),
            context,
        }
    }

    fn retire(mut self, disposition: Retirement) -> Result<(), TimedWaitError> {
        let Some(timer) = self.timer.take() else {
            hyper::debug::invariant_failure("task::timeout::retire invariant")
        };
        retire_timeout(timer, &self.context, disposition)
    }

    /// Retires the timer after the scheduler selected `outcome`.
    ///
    /// A timeout winner must join its callback. Notification and cancellation
    /// cancel the exact source-CPU handle or join a callback which detached it
    /// concurrently. This operation may wait for an in-flight callback and
    /// therefore must run without object, Process, or scheduler locks held.
    pub(crate) fn retire_after(self, outcome: WaitOutcome) -> Result<(), TimedWaitError> {
        let disposition = if outcome == WaitOutcome::TimedOut {
            Retirement::CallbackCompleted
        } else {
            Retirement::CancelOrJoin
        };
        self.retire(disposition)
    }
}

impl Drop for ArmedTimeout {
    fn drop(&mut self) {
        if self.timer.is_some() {
            // Destruction may occur under an arbitrary lock or interrupt-mask
            // state. Logging could deadlock before protecting the callback
            // context, so only the crash-safe panic path may report the failure.
            hyper::debug::invariant_failure("task::timeout::drop invariant")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retirement {
    CallbackCompleted,
    CancelOrJoin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimedWaitError {
    Allocation,
    Scheduler(scheduler::Error),
    Time(crate::time::Error),
    TimerCleanup(crate::time::Error),
}

impl From<scheduler::Error> for TimedWaitError {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::time::Error> for TimedWaitError {
    fn from(error: crate::time::Error) -> Self {
        Self::Time(error)
    }
}

impl WaitQueue {
    /// Blocks until notification or until `deadline` is reached.
    ///
    /// A deadline at or before the current monotonic counter completes without
    /// blocking. Notifications are not retained: callers which need a durable
    /// condition should use a synchronization primitive that owns both state
    /// and a wait queue. `deadline` must be within the counter's unambiguous
    /// wrapping half-range, matching the software timer queue contract.
    pub fn wait_until(&self, deadline: u64) -> Result<WaitOutcome, TimedWaitError> {
        if deadline_reached(crate::time::monotonic_ticks(), deadline) {
            return Ok(WaitOutcome::TimedOut);
        }
        scheduler::ensure_sleepable()?;

        // Allocate the callback owner before arming the Thread. Allocation is
        // deliberately outside the scheduler wait transaction.
        let timeout = PreparedTimeout::try_new()?;
        let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
        let timeout = match timeout.arm(registration.ticket(), deadline) {
            Ok(timeout) => timeout,
            Err(error) => {
                finish_unscheduled(registration);
                return Err(error);
            }
        };

        let prepared = match scheduler::prepare_registered_park(self, registration) {
            Ok(prepared) => prepared,
            Err(error) => {
                // prepare_registered_park may restore local IRQs before this
                // branch runs. `timeout` still owns the Box throughout that
                // window, so expiry may safely complete as a losing resolver;
                // retirement then either cancels or joins that callback.
                timeout.retire(Retirement::CancelOrJoin)?;
                return Err(error.into());
            }
        };
        let outcome = prepared.complete();
        timeout.retire_after(outcome)?;
        Ok(outcome)
    }

    /// Blocks until notified or until `nanoseconds` of monotonic time elapse.
    pub fn wait_for(&self, nanoseconds: u64) -> Result<WaitOutcome, TimedWaitError> {
        let deadline = crate::time::deadline_after(nanoseconds)?;
        self.wait_until(deadline)
    }
}

fn finish_unscheduled(registration: scheduler::WaitRegistration) {
    match scheduler::finish_wait(registration) {
        Ok(None) => {}
        Ok(Some(outcome)) => wait_invariant("unscheduled wait was resolved", Some(outcome)),
        Err(error) => {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: unscheduled wait cleanup failed: {error:?}"
            ));
        }
    }
}

fn retire_timeout(
    timer: crate::time::TimerHandle,
    timeout: &TimeoutContext,
    disposition: Retirement,
) -> Result<(), TimedWaitError> {
    if disposition == Retirement::CallbackCompleted {
        timeout.wait_for_callback();
        return Ok(());
    }

    match crate::time::cancel_irq_timer(timer) {
        Ok(()) => Ok(()),
        Err(crate::time::Error::TimerQueue(crate::time::queue::TimerQueueError::InvalidHandle)) => {
            // Expiry detached the node before cancellation acquired its source
            // queue. The callback is the remaining owner of the raw context.
            timeout.wait_for_callback();
            Ok(())
        }
        Err(error @ crate::time::Error::Architecture(_)) => {
            // Cancellation detached the node before local comparator
            // reprogramming failed, so the callback cannot access the context.
            Err(TimedWaitError::TimerCleanup(error))
        }
        Err(error) => {
            // These lifecycle/identity failures occur before proven removal.
            // Returning could free a callback context which is still queued.
            if timeout.callback_complete.load(Ordering::Acquire) {
                return Err(TimedWaitError::TimerCleanup(error));
            }
            // The owner remains live because this path never returns. Use the
            // lock-independent crash coordinator so no other CPU can continue
            // against an ambiguous timer lifecycle.
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: timed wait retirement failed before proven detach: {error:?}"
            ));
        }
    }
}

fn expire_wait(_event: crate::time::TimerEvent, context: usize) {
    // SAFETY: wait_until owns this boxed context until it either cancels the
    // timer or observes callback_complete with Acquire ordering.
    let timeout = unsafe { &*core::ptr::with_exposed_provenance::<TimeoutContext>(context) };
    let result = match timeout.ticket {
        Some(ticket) => scheduler::resolve_wait(ticket, WaitOutcome::TimedOut),
        None => Err(scheduler::Error::InvalidWaitRegistration),
    };
    match result {
        Ok(resolution) if !resolution.made_ready || resolution.won => {
            // Publish completion only after classifying a benign winning or
            // stale losing callback. Publishing it on an invariant failure
            // could let the waiter retire the Box and continue before global
            // crash coordination has stopped every CPU.
            timeout.callback_complete.store(true, Ordering::Release);
        }
        Ok(_) => wait_invariant("timeout published Ready without winning", None),
        Err(error) => {
            // A scheduler error is not a benign stale losing callback. Leave
            // completion unpublished so the raw owner cannot be retired, then
            // stop every CPU before any waiter can continue against invalid
            // scheduler state.
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: timed wait resolution failed: {error:?}"
            ));
        }
    }
}

fn wait_invariant(message: &str, outcome: Option<WaitOutcome>) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: timed wait invariant failed: {message}; outcome={outcome:?}"
    ))
}
