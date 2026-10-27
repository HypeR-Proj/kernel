// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owned, reusable timers delivered by the source CPU's normal-context worker.
//!
//! Callback data belongs to the callout, never to an untracked integer pointer
//! supplied by its caller. A callout must be drained before its owner drops it;
//! shutdown additionally closes admission against concurrent or callback rearm.

mod lifecycle;
mod ready;

use alloc::boxed::Box;
use hyper::cpu::CpuIndex;
use hyper::mm::{FallibleArc, try_box};

use super::{Error, TimerEvent};
use lifecycle::Core;
pub(super) use ready::ReadyQueue;

/// Nonblocking cancellation does not join an already claimed delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancelOutcome {
    Idle,
    Cancelled,
    Running,
}

struct Context<T> {
    data: T,
    callback: fn(&T, CalloutRef<'_>, TimerEvent),
}

/// A preallocated, source-CPU-bound callout with owned callback data.
///
/// Callbacks run with interrupts and preemption enabled and with no timer locks
/// held. They must be short and must not block; use a separate worker for I/O or
/// other blocking work. The deadline is an earliest execution time, not a
/// guarantee that the timer worker has already been scheduled at that instant.
pub(crate) struct Callout<T: Send + Sync + 'static> {
    core: FallibleArc<Core>,
    _context: Box<Context<T>>,
}

#[cfg_attr(
    not(all(feature = "kernel-test", feature = "kernel-self-test")),
    allow(
        dead_code,
        reason = "general callout lifecycle is exercised by kernel self-tests"
    )
)]
impl<T: Send + Sync + 'static> Callout<T> {
    pub(crate) fn try_new(
        data: T,
        callback: fn(&T, CalloutRef<'_>, TimerEvent),
    ) -> Result<Self, Error> {
        let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpuIndex)?;
        Self::try_new_on(cpu, data, callback)
    }

    /// Binds lifetime ownership to one admitted CPU, independently of the
    /// caller's later migration. Remote resets prompt its comparator directly.
    pub(crate) fn try_new_on(
        cpu: CpuIndex,
        data: T,
        callback: fn(&T, CalloutRef<'_>, TimerEvent),
    ) -> Result<Self, Error> {
        if cpu.get() >= crate::cpu::participating_cpu_count().unwrap_or(0) {
            return Err(Error::InvalidCpuIndex);
        }
        super::timers::with_callout_queue(cpu, |_, _| ())?;
        let context = try_box(Context { data, callback })
            .map_err(|_| Error::TimerQueue(crate::time::queue::TimerQueueError::Allocation))?;
        let pointer = core::ptr::from_ref(context.as_ref()).expose_provenance();
        let core = FallibleArc::try_new(Core::try_new(cpu, invoke::<T>, pointer)?)
            .map_err(|_| Error::TimerQueue(crate::time::queue::TimerQueueError::Allocation))?;
        Ok(Self {
            core,
            _context: context,
        })
    }

    /// Arms an idle callout, or one next invocation from its running callback.
    pub(crate) fn arm_at(&self, deadline: u64) -> Result<(), Error> {
        self.core.set_deadline(&self.core, deadline, false)
    }

    pub(crate) fn arm_after(&self, nanoseconds: u64) -> Result<(), Error> {
        self.arm_at(super::deadline_after(nanoseconds)?)
    }

    /// Replaces any pending deadline. If a callback is already running, its
    /// successor is installed only after it returns; callbacks never overlap.
    pub(crate) fn reset(&self, deadline: u64) -> Result<(), Error> {
        self.core.set_deadline(&self.core, deadline, true)
    }

    pub(crate) fn cancel(&self) -> Result<CancelOutcome, Error> {
        self.core.cancel(false).map(|(outcome, _)| outcome)
    }

    /// Cancels pending work and joins the claimed generation. Callers must
    /// coordinate concurrent rearming; use shutdown for permanent retirement.
    /// Must not be called from an IRQ, any timer callback, or under a lock a
    /// callback needs. It never waits while holding a timer lock.
    pub(crate) fn drain(&self) -> Result<(), Error> {
        self.core.synchronize(false)
    }

    /// Permanently rejects rearm before cancelling and joining all delivery.
    /// After success the callback context may be destroyed safely.
    pub(crate) fn shutdown_sync(&self) -> Result<(), Error> {
        self.core.synchronize(true)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn is_ready_for_test(&self) -> bool {
        self.core.is_ready_for_test()
    }
}

impl<T: Send + Sync + 'static> Drop for Callout<T> {
    fn drop(&mut self) {
        if !self.core.is_idle() {
            // Fail before either the typed context or its queue-borrowed Core
            // can be released. Teardown is explicit, never a blocking Drop.
            hyper::debug::invariant_failure("live callout dropped without draining");
        }
    }
}

/// Callback-scoped control without a synchronous self-wait operation.
#[derive(Clone, Copy)]
#[cfg_attr(
    not(all(feature = "kernel-test", feature = "kernel-self-test")),
    allow(
        dead_code,
        reason = "callback rearm control is exercised by kernel self-tests"
    )
)]
pub(crate) struct CalloutRef<'a> {
    core: &'a Core,
    owner: &'a FallibleArc<Core>,
}

#[cfg_attr(
    not(all(feature = "kernel-test", feature = "kernel-self-test")),
    allow(
        dead_code,
        reason = "callback rearm control is exercised by kernel self-tests"
    )
)]
impl CalloutRef<'_> {
    pub(crate) fn reset(self, deadline: u64) -> Result<(), Error> {
        self.core.set_deadline(self.owner, deadline, true)
    }

    pub(crate) fn arm_after(self, nanoseconds: u64) -> Result<(), Error> {
        self.core
            .set_deadline(self.owner, super::deadline_after(nanoseconds)?, false)
    }

    pub(crate) fn cancel(self) -> Result<CancelOutcome, Error> {
        self.core.cancel(false).map(|(outcome, _)| outcome)
    }
}

fn invoke<T: Send + Sync + 'static>(pointer: usize, owner: &FallibleArc<Core>, event: TimerEvent) {
    // SAFETY: the owning Callout retains this typed allocation throughout the
    // Running state. Drop fails before releasing it while any delivery remains
    // active. This function returns before completion permits the owner to drop.
    let context = unsafe { &*core::ptr::with_exposed_provenance::<Context<T>>(pointer) };
    (context.callback)(&context.data, CalloutRef { core: owner, owner }, event);
}

pub(super) fn run_one(cpu: CpuIndex) -> bool {
    lifecycle::run_one(cpu)
}

pub(super) fn has_ready(cpu: CpuIndex) -> bool {
    super::timers::with_callout_queue(cpu, |_, ready| !ready.is_empty())
        .unwrap_or_else(|_| hyper::debug::invariant_failure("callout worker queue unavailable"))
}
