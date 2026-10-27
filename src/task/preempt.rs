// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-CPU preemption requests and entry-depth accounting.
//!
//! This module owns only the policy state used to decide whether a safe
//! scheduling point should consult the scheduler. It never switches context.
//! Remote CPUs may publish coalesced requests, while only the owning CPU may
//! change its disable and IRQ nesting depths.

use core::marker::PhantomData;

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{AtomicU32, Ordering, compiler_fence};

use super::reschedule::PendingReschedule;

const OFFLINE: u32 = 0;
const PREPARING: u32 = 1;
const ONLINE: u32 = 2;

/// Trace vocabulary for a committed request, not an exclusive cause: requests
/// may coalesce and a later selection can retain the currently running Thread.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum RescheduleSource {
    Wakeup = 1,
    Deadline = 2,
    Stop = 3,
    Policy = 4,
    Migration = 5,
    Balance = 6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    AlreadyOnline,
    Offline,
    InvalidCpu,
    DisableDepthOverflow,
    DisableDepthUnderflow,
    IrqDepthOverflow,
    IrqDepthUnderflow,
    WrongCpu,
}

impl Error {
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::AlreadyOnline => "per-CPU preemption state was initialized twice",
            Self::Offline => "per-CPU preemption state is offline",
            Self::InvalidCpu => "invalid logical CPU for preemption accounting",
            Self::DisableDepthOverflow => "preemption-disable depth overflow",
            Self::DisableDepthUnderflow => "preemption-disable depth underflow",
            Self::IrqDepthOverflow => "IRQ nesting overflow",
            Self::IrqDepthUnderflow => "IRQ nesting underflow",
            Self::WrongCpu => "preemption guard completed on the wrong CPU",
        }
    }
}

struct PreemptionState {
    lifecycle: AtomicU32,
    disable_depth: AtomicU32,
    irq_depth: AtomicU32,
    pending: PendingReschedule,
}

impl PreemptionState {
    const fn new() -> Self {
        Self {
            lifecycle: AtomicU32::new(OFFLINE),
            disable_depth: AtomicU32::new(0),
            irq_depth: AtomicU32::new(0),
            pending: PendingReschedule::new(),
        }
    }
}

static PREEMPTION: PerCpu<PreemptionState> =
    PerCpu::new([const { PreemptionState::new() }; hyper::cpu::MAX_CPUS]);

pub(crate) fn prepare_cpu(cpu: CpuIndex) -> Result<CpuPreemptionReservation, Error> {
    let state = &PREEMPTION[cpu];
    state
        .lifecycle
        .compare_exchange(OFFLINE, PREPARING, Ordering::Acquire, Ordering::Relaxed)
        .map_err(|_| Error::AlreadyOnline)?;
    Ok(CpuPreemptionReservation {
        cpu,
        committed: false,
        not_send: PhantomData,
    })
}

/// Rollback capability for one unpublished per-CPU preemption state.
pub(crate) struct CpuPreemptionReservation {
    cpu: CpuIndex,
    committed: bool,
    not_send: PhantomData<*mut ()>,
}

impl CpuPreemptionReservation {
    /// Publishes the zero-depth state after the scheduler CPU record exists.
    pub(crate) fn commit(mut self) {
        PREEMPTION[self.cpu]
            .lifecycle
            .store(ONLINE, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for CpuPreemptionReservation {
    fn drop(&mut self) {
        if !self.committed {
            PREEMPTION[self.cpu]
                .lifecycle
                .store(OFFLINE, Ordering::Release);
        }
    }
}

/// Publishes a reschedule request to `cpu`.
///
/// Release ordering makes scheduler queue publication visible before the
/// target CPU observes the request with Acquire ordering. Requests coalesce;
/// clearing occurs only while the scheduler lock serializes the decision. A
/// `true` result elects this caller to send an explicit target notification.
/// A newly pending local request inside IRQ dispatch returns `false`: that
/// still-active interrupt already guarantees the target's IRQ-tail evaluation.
pub(crate) fn request(cpu: CpuIndex, _source: RescheduleSource) -> Result<bool, Error> {
    // SAFETY: Keep CPU identification, local IRQ-depth observation, and pending
    // publication in one masked interval. Otherwise IRQ-tail migration could
    // make this caller read another CPU's IRQ depth and suppress notification
    // after that interrupt has already completed. No scheduling occurs here.
    let _mask = unsafe { hyper::sync::InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    // Short-circuiting keeps remote callers from observing locally owned IRQ
    // depth. Perform every fallible check before electing the sole notifier.
    let notify = current_cpu()? != cpu || state.irq_depth.load(Ordering::Relaxed) == 0;
    let newly_pending = state.pending.publish();
    let notify = newly_pending && notify;
    // Repeated publications coalesce into the same scheduling decision.
    // Internal balancing prompts are maintenance, not workload wakeups; their
    // actual placement changes and switches have their own observations.
    #[cfg(feature = "kernel-trace")]
    if newly_pending && !matches!(_source, RescheduleSource::Balance) {
        crate::tracepoint!(
            Scheduler,
            "sched_request",
            target_cpu = cpu.get() as u64,
            reason = _source as u64,
            notify = notify as u64,
            thread = crate::diagnostics::trace::actor()
        );
    }
    Ok(notify)
}

pub(crate) fn pending(cpu: CpuIndex) -> Result<bool, Error> {
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    Ok(state.pending.is_pending())
}

/// Takes all requests observed while the scheduler lock is held.
///
/// A concurrent request after this exchange remains pending for the next safe
/// point. A request observed by the exchange already has its ready-queue state
/// serialized by the same scheduler lock.
pub(super) fn take_pending_locked(cpu: CpuIndex) -> Result<bool, Error> {
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    Ok(state.pending.take())
}

pub(crate) fn can_reschedule(cpu: CpuIndex) -> Result<bool, Error> {
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    // Depth ownership is CPU-local. Guard/IRQ boundary compiler fences order
    // protected accesses; these atomics provide nesting and overflow safety,
    // not inter-CPU memory publication.
    Ok(state.disable_depth.load(Ordering::Relaxed) == 0
        && state.irq_depth.load(Ordering::Relaxed) == 0)
}

/// Tests whether an outermost IRQ exit has deferred scheduling work.
///
/// The caller must complete its [`IrqGuard`] first and keep local IRQs masked.
/// The scheduler lock later serializes the final decision and consumes the
/// coalesced request.
pub(crate) fn should_reschedule_after_irq() -> Result<bool, Error> {
    let cpu = current_cpu()?;
    Ok(pending(cpu)? && can_reschedule(cpu)?)
}

/// Disables asynchronous preemption on the calling CPU.
///
/// Dropping the guard only restores accounting. Call
/// `scheduler::preempt_enable_and_reschedule` when the release point is also a
/// deliberate scheduling point.
pub(crate) fn disable() -> Result<PreemptionGuard, Error> {
    // Closing the identification-to-publication window is essential: an IRQ
    // tail could otherwise migrate this continuation after `current_cpu()` but
    // before the old CPU's depth is incremented.
    // SAFETY: The guard remains lexical and is restored before this function
    // returns. A nonzero disable depth pins the continuation afterwards.
    let interrupt_mask =
        unsafe { hyper::sync::InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    let cpu = current_cpu()?;
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    increment(&state.disable_depth, Error::DisableDepthOverflow)?;
    // Do not let protected accesses move above the local preemption boundary.
    // CPU-local depth accounting needs no hardware fence or global ordering.
    compiler_fence(Ordering::Acquire);
    let guard = PreemptionGuard {
        cpu,
        active: true,
        not_send: PhantomData,
    };
    drop(interrupt_mask);
    Ok(guard)
}

pub(crate) struct PreemptionGuard {
    cpu: CpuIndex,
    active: bool,
    not_send: PhantomData<*mut ()>,
}

impl PreemptionGuard {
    /// Releases one nesting level and reports whether preemption became
    /// enabled on this CPU.
    pub(crate) fn release(mut self) -> Result<bool, Error> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<bool, Error> {
        if !self.active {
            return Ok(false);
        }
        if current_cpu()? != self.cpu {
            return Err(Error::WrongCpu);
        }
        // Complete protected accesses before making this continuation
        // preemptible again. This is a compiler boundary, not inter-CPU
        // publication; the scheduler lock supplies that synchronization.
        compiler_fence(Ordering::Release);
        let previous = decrement(
            &PREEMPTION[self.cpu].disable_depth,
            Error::DisableDepthUnderflow,
        )?;
        self.active = false;
        Ok(previous == 1)
    }
}

impl Drop for PreemptionGuard {
    fn drop(&mut self) {
        if self.release_inner().is_err() {
            // Guard destruction may occur under arbitrary locks. Logging can
            // deadlock before preserving the non-preemptible fail-stop state.
            hyper::debug::invariant_failure("task::preempt::drop invariant")
        }
    }
}

/// Records one normal maskable-interrupt entry on the calling CPU.
pub(crate) fn enter_irq() -> Result<IrqGuard, Error> {
    let cpu = current_cpu()?;
    let state = &PREEMPTION[cpu];
    ensure_online(state)?;
    increment(&state.irq_depth, Error::IrqDepthOverflow)?;
    // Interrupt entry is a one-way compiler boundary for the interrupted
    // continuation. Architecture exception entry owns the hardware ordering.
    compiler_fence(Ordering::Acquire);
    Ok(IrqGuard {
        cpu,
        active: true,
        not_send: PhantomData,
    })
}

pub(crate) struct IrqGuard {
    cpu: CpuIndex,
    active: bool,
    not_send: PhantomData<*mut ()>,
}

impl IrqGuard {
    /// Completes IRQ accounting and reports an outermost IRQ exit.
    ///
    /// Only the outermost exit may enter the architecture IRQ-tail scheduling
    /// seam. A nested exit still has an IRQ-stack owner above it.
    pub(crate) fn complete(mut self) -> Result<bool, Error> {
        self.complete_inner()
    }

    fn complete_inner(&mut self) -> Result<bool, Error> {
        if !self.active {
            return Ok(false);
        }
        if current_cpu()? != self.cpu {
            return Err(Error::WrongCpu);
        }
        // Keep IRQ-handler accesses before the outermost-exit decision without
        // imposing a hardware barrier on every interrupt.
        compiler_fence(Ordering::Release);
        let previous = decrement(&PREEMPTION[self.cpu].irq_depth, Error::IrqDepthUnderflow)?;
        self.active = false;
        Ok(previous == 1)
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.complete_inner().is_err() {
            // IRQ-exit accounting is still live and Drop may hold unrelated
            // locks, so report only through the lock-free panic boundary.
            hyper::debug::invariant_failure("task::preempt::drop invariant")
        }
    }
}

fn increment(counter: &AtomicU32, overflow: Error) -> Result<(), Error> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).ok_or(overflow)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

/// Returns the value before decrementing it.
fn decrement(counter: &AtomicU32, underflow: Error) -> Result<u32, Error> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_sub(1).ok_or(underflow)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

fn ensure_online(state: &PreemptionState) -> Result<(), Error> {
    (state.lifecycle.load(Ordering::Acquire) == ONLINE)
        .then_some(())
        .ok_or(Error::Offline)
}

fn current_cpu() -> Result<CpuIndex, Error> {
    crate::cpu::current_index().ok_or(Error::InvalidCpu)
}
