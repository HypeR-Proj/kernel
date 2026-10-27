// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Source-lock-serialized callout admission, delivery, and retirement.

use core::cell::UnsafeCell;
use core::ptr::NonNull;

use hyper::cpu::CpuIndex;
use hyper::mm::FallibleArc;
use {
    crate::time::queue::OwnedDeadlineQueue, crate::time::queue::ReservedTimerCallbacks,
    crate::time::queue::ReservedTimerNode,
};

use {
    crate::task::WaitMobility, crate::task::WaitOutcome, crate::task::WaitQueue,
    crate::task::scheduler,
};

use super::super::{Error, TimerEvent, TimerHandle, timers, worker};
use super::CancelOutcome;
use super::ready::{Links, ReadyQueue, WorkKind};

#[derive(Clone, Copy)]
struct Arm {
    generation: u64,
    deadline: u64,
}

enum ExpiryAction {
    Deliver,
    Cancel,
    Replace(Arm),
}

enum Phase {
    Idle(ReservedTimerNode),
    Armed {
        arm: Arm,
        handle: TimerHandle,
    },
    // The IRQ owns the detached node until its recycle hook. Cancellation can
    // suppress delivery but must join this phase before releasing context.
    Expiring {
        arm: Arm,
        event: TimerEvent,
        action: ExpiryAction,
    },
    Ready {
        arm: Arm,
        event: TimerEvent,
        node: ReservedTimerNode,
    },
    Running {
        arm: Arm,
        node: ReservedTimerNode,
        next: Option<Arm>,
    },
    // Exists only while the source lock transfers an owned node between states.
    Transition,
}

impl Phase {
    fn generation(&self) -> Option<u64> {
        match self {
            Self::Idle(_) => None,
            Self::Armed { arm, .. }
            | Self::Expiring { arm, .. }
            | Self::Ready { arm, .. }
            | Self::Running { arm, .. } => Some(arm.generation),
            Self::Transition => invariant("observable callout transition"),
        }
    }

    fn has_pending(&self) -> bool {
        match self {
            Self::Idle(_) => false,
            Self::Running { next, .. } => next.is_some(),
            Self::Armed { .. } | Self::Expiring { .. } | Self::Ready { .. } => true,
            Self::Transition => invariant("observable callout transition"),
        }
    }
}

struct State {
    closed: bool,
    generation: u64,
    phase: Phase,
    // A queued or ready arm owns one Core reference. Worker claim transfers it
    // to the running continuation. Explicit cancellation breaks this edge.
    execution: Option<FallibleArc<Core>>,
    wake_execution: Option<FallibleArc<Core>>,
}

pub(super) struct Core {
    cpu: CpuIndex,
    invoke: fn(usize, &FallibleArc<Core>, TimerEvent),
    context: usize,
    state: UnsafeCell<State>,
    pub(super) ready: UnsafeCell<Links>,
    pub(super) wake: UnsafeCell<Links>,
    waiters: WaitQueue,
}

// SAFETY: state and ready links are accessed only under this Core's immutable
// source CPU timer lock. Callback data is separately owned by Callout<T>, whose
// Send + Sync bounds and live-drop check dominate all erased callback accesses.
unsafe impl Send for Core {}
// SAFETY: the same source lock serializes every shared mutable field.
unsafe impl Sync for Core {}

impl Core {
    pub(super) fn try_new(
        cpu: CpuIndex,
        invoke: fn(usize, &FallibleArc<Core>, TimerEvent),
        context: usize,
    ) -> Result<Self, Error> {
        Ok(Self {
            cpu,
            invoke,
            context,
            state: UnsafeCell::new(State {
                closed: false,
                generation: 0,
                phase: Phase::Idle(ReservedTimerNode::try_new()?),
                execution: None,
                wake_execution: None,
            }),
            ready: UnsafeCell::new(Links::default()),
            wake: UnsafeCell::new(Links::default()),
            waiters: WaitQueue::new(),
        })
    }

    pub(super) fn is_idle(&self) -> bool {
        match timers::with_callout_queue(self.cpu, |_, _| {
            // SAFETY: with_callout_queue holds this immutable source lock.
            matches!(unsafe { &(*self.state.get()).phase }, Phase::Idle(_))
        }) {
            Ok(idle) => idle,
            Err(_) => invariant("callout source queue vanished"),
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn is_ready_for_test(&self) -> bool {
        match timers::with_callout_queue(self.cpu, |_, _| {
            // SAFETY: the phase is read under its source queue lock.
            matches!(unsafe { &(*self.state.get()).phase }, Phase::Ready { .. })
        }) {
            Ok(ready) => ready,
            Err(_) => invariant("callout source queue vanished"),
        }
    }

    pub(super) fn set_deadline(
        &self,
        owner: &FallibleArc<Self>,
        deadline: u64,
        replace: bool,
    ) -> Result<(), Error> {
        if !worker::is_ready(self.cpu) {
            return Err(Error::CalloutWorkerUnavailable);
        }
        let result = timers::with_callout_queue(self.cpu, |queue, ready| {
            // SAFETY: all state and queue membership share this source lock.
            let state = unsafe { &mut *self.state.get() };
            if state.closed {
                return Err(Error::CalloutClosed);
            }
            if !replace && state.phase.has_pending() {
                return Err(Error::CalloutBusy);
            }
            let generation = state
                .generation
                .checked_add(1)
                .ok_or(Error::CalloutGenerationExhausted)?;
            state.generation = generation;
            let next = Arm {
                generation,
                deadline,
            };
            let mut wake = false;
            let old = core::mem::replace(&mut state.phase, Phase::Transition);
            state.phase = match old {
                Phase::Idle(node) => {
                    state.execution = Some(owner.clone());
                    self.schedule(queue, node, next)
                }
                Phase::Armed { handle, .. } => {
                    if queue
                        .reschedule_reserved(handle, deadline, super::super::monotonic_ticks())
                        .is_err()
                    {
                        invariant("armed callout missing from deadline queue");
                    }
                    Phase::Armed { arm: next, handle }
                }
                Phase::Ready { node, .. } => {
                    // SAFETY: this exact Ready membership and its execution
                    // owner remain protected by the same source lock.
                    unsafe { ready.remove(NonNull::from(self)) };
                    self.defer_wake(state, ready, owner.clone());
                    wake = true;
                    self.schedule(queue, node, next)
                }
                Phase::Expiring { arm, event, .. } => Phase::Expiring {
                    arm,
                    event,
                    action: ExpiryAction::Replace(next),
                },
                Phase::Running { arm, node, .. } => Phase::Running {
                    arm,
                    node,
                    next: Some(next),
                },
                Phase::Transition => invariant("nested callout transition"),
            };
            Ok(wake)
        })?;
        if result? {
            // A retired Ready generation can have a draining caller waiting
            // for replacement. Ordinary future arms do not wake the worker.
            worker::request(self.cpu);
        }
        Ok(())
    }

    fn schedule(&self, queue: &mut OwnedDeadlineQueue, node: ReservedTimerNode, arm: Arm) -> Phase {
        let pointer = core::ptr::from_ref(self).expose_provenance();
        let pending = node.prepare(
            arm.deadline,
            ReservedTimerCallbacks {
                callback: detached_expiry,
                context: pointer,
                claim: claim_expiry,
                claim_context: pointer,
                recycle: recycle_expiry,
                recycle_context: pointer,
            },
        );
        let handle = queue.insert_reserved(pending, super::super::monotonic_ticks());
        Phase::Armed { arm, handle }
    }

    pub(super) fn cancel(&self, close: bool) -> Result<(CancelOutcome, Option<u64>), Error> {
        let (outcome, generation, wake, retired) =
            timers::with_callout_queue(self.cpu, |queue, ready| {
                // SAFETY: this source lock protects phase and membership together.
                let state = unsafe { &mut *self.state.get() };
                state.closed |= close;
                let old = core::mem::replace(&mut state.phase, Phase::Transition);
                let defer_retirement = matches!(&old, Phase::Ready { .. });
                let (phase, outcome, generation) = match old {
                    Phase::Idle(node) => (Phase::Idle(node), CancelOutcome::Idle, None),
                    Phase::Armed { handle, .. } => {
                        let node = match queue.cancel_reserved(handle) {
                            Ok(node) => node,
                            Err(_) => invariant("cancelled callout missing from deadline queue"),
                        };
                        (Phase::Idle(node), CancelOutcome::Cancelled, None)
                    }
                    Phase::Ready { node, .. } => {
                        // SAFETY: Ready owns one exact FIFO link under this lock.
                        unsafe { ready.remove(NonNull::from(self)) };
                        (Phase::Idle(node), CancelOutcome::Cancelled, None)
                    }
                    Phase::Expiring { arm, event, .. } => (
                        Phase::Expiring {
                            arm,
                            event,
                            action: ExpiryAction::Cancel,
                        },
                        CancelOutcome::Running,
                        Some(arm.generation),
                    ),
                    Phase::Running { arm, node, .. } => (
                        Phase::Running {
                            arm,
                            node,
                            next: None,
                        },
                        CancelOutcome::Running,
                        Some(arm.generation),
                    ),
                    Phase::Transition => invariant("nested callout cancellation"),
                };
                state.phase = phase;
                let mut retired = if generation.is_none() {
                    state.execution.take()
                } else {
                    None
                };
                let wake = defer_retirement && retired.is_some();
                if defer_retirement && let Some(retired) = retired.take() {
                    self.defer_wake(state, ready, retired);
                }
                (outcome, generation, wake, retired)
            })?;
        // An Armed cancellation has no claimed-generation waiter. This call
        // borrows the public Callout's owner (or a running callback's owner),
        // so releasing its extra execution reference cannot destroy Core.
        // Earlier-generation wake membership retains its own independent pin.
        drop(retired);
        if wake {
            worker::request(self.cpu);
        }
        Ok((outcome, generation))
    }

    fn defer_wake(&self, state: &mut State, ready: &mut ReadyQueue, owner: FallibleArc<Core>) {
        if state.wake_execution.is_none() {
            state.wake_execution = Some(owner);
            // SAFETY: the source lock protects this separate wake membership;
            // the published owner retains Core until a worker consumes it.
            unsafe { ready.push_wake(NonNull::from(self)) };
        }
        // A coalesced extra reference cannot be the last: the existing wake
        // owner remains retained. No destruction can occur under this lock.
    }

    pub(super) fn synchronize(&self, close: bool) -> Result<(), Error> {
        scheduler::ensure_sleepable().map_err(Error::CalloutWait)?;
        if worker::is_current_worker().map_err(Error::CalloutWait)? {
            return Err(Error::CalloutCallbackContext);
        }
        let (_, generation) = self.cancel(close)?;
        let Some(generation) = generation else {
            return Ok(());
        };
        loop {
            // SAFETY: the returned mask is consumed by the prepared park, or
            // dropped below on completion/error. The source lock joins the
            // condition check and registration against callback completion.
            let (prepared, mask) = unsafe {
                timers::with_callout_queue_mask_retained(self.cpu, || {
                    let state = &*self.state.get();
                    if state.phase.generation() != Some(generation) {
                        return Ok(None);
                    }
                    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
                    scheduler::prepare_registered_park_locked(&self.waiters, registration).map(Some)
                })
            };
            let prepared = prepared.map_err(Error::CalloutWait)?;
            let Some(prepared) = prepared else {
                drop(mask);
                return Ok(());
            };
            let outcome = prepared.retain_mask(mask).complete();
            if outcome != WaitOutcome::Notified {
                return Err(Error::CalloutWait(
                    scheduler::Error::InvalidWaitRegistration,
                ));
            }
        }
    }
}

fn detached_expiry(_event: TimerEvent, _context: usize) {
    // Claim runs under the deadline lock. Actual ready publication waits for
    // recycle to return the reusable node after the IRQ delivery is detached.
}

fn claim_expiry(event: TimerEvent, pointer: usize) {
    // SAFETY: a queued execution reference retains Core. OwnedDeadlineQueue
    // invokes claim under its source timer lock, before releasing the node.
    let core = unsafe { &*core::ptr::with_exposed_provenance::<Core>(pointer) };
    // SAFETY: claim executes under this Core's source deadline queue lock.
    let state = unsafe { &mut *core.state.get() };
    let old = core::mem::replace(&mut state.phase, Phase::Transition);
    let Phase::Armed { arm, handle } = old else {
        invariant("callout expiry claimed outside armed state");
    };
    if handle != event.handle {
        invariant("callout expiry generation mismatch");
    }
    state.phase = Phase::Expiring {
        arm,
        event,
        action: ExpiryAction::Deliver,
    };
}

fn recycle_expiry(node: ReservedTimerNode, pointer: usize) {
    // SAFETY: Expiring retains its execution reference until this function
    // transfers it to Ready/Armed, or takes an owned reference for final wake.
    let core = unsafe { &*core::ptr::with_exposed_provenance::<Core>(pointer) };
    let cpu = core.cpu;
    let result = timers::with_callout_queue(cpu, |queue, ready| {
        // SAFETY: state and ready membership share this source lock.
        let state = unsafe { &mut *core.state.get() };
        let old = core::mem::replace(&mut state.phase, Phase::Transition);
        let Phase::Expiring { arm, event, action } = old else {
            invariant("callout recycled outside expiry state");
        };
        match action {
            ExpiryAction::Deliver => {
                state.phase = Phase::Ready { arm, event, node };
                // SAFETY: Ready retains an execution pin until remove/claim.
                unsafe { ready.push(NonNull::from(core)) };
                true
            }
            ExpiryAction::Cancel => {
                state.phase = Phase::Idle(node);
                let owner = match state.execution.take() {
                    Some(owner) => owner,
                    None => invariant("expiring callout lost its execution owner"),
                };
                core.defer_wake(state, ready, owner);
                true
            }
            ExpiryAction::Replace(next) => {
                state.phase = core.schedule(queue, node, next);
                let owner = match state.execution.as_ref() {
                    Some(owner) => owner.clone(),
                    None => invariant("replaced callout lost its execution owner"),
                };
                core.defer_wake(state, ready, owner);
                true
            }
        }
    });
    let ready = match result {
        Ok(result) => result,
        Err(_) => invariant("callout recycle source vanished"),
    };
    if ready {
        worker::request(cpu);
    }
}

enum Work {
    Wake(FallibleArc<Core>),
    Callback(FallibleArc<Core>, TimerEvent),
}

pub(super) fn run_one(cpu: CpuIndex) -> bool {
    let running = timers::with_callout_queue(cpu, |_, ready| {
        // SAFETY: pop and Ready-to-Running claim share the source lock, so a
        // concurrent cancel cannot detach or free the selected allocation.
        let (pointer, kind) = unsafe { ready.pop() }?;
        // SAFETY: the selected membership retains Core until claim takes it.
        let core = unsafe { pointer.as_ref() };
        // SAFETY: state access remains within this source-lock claim.
        let state = unsafe { &mut *core.state.get() };
        if matches!(kind, WorkKind::Wake) {
            let owner = match state.wake_execution.take() {
                Some(owner) => owner,
                None => invariant("callout wake lost its retained owner"),
            };
            return Some(Work::Wake(owner));
        }
        let old = core::mem::replace(&mut state.phase, Phase::Transition);
        let Phase::Ready { arm, event, node } = old else {
            invariant("ready callout lost its delivery state");
        };
        state.phase = Phase::Running {
            arm,
            node,
            next: None,
        };
        let owner = match state.execution.take() {
            Some(owner) => owner,
            None => invariant("ready callout lost its execution owner"),
        };
        Some(Work::Callback(owner, event))
    });
    let running = match running {
        Ok(running) => running,
        Err(_) => invariant("callout worker source vanished"),
    };
    let Some(running) = running else {
        return false;
    };
    match running {
        Work::Wake(owner) => wake_waiters(&owner),
        Work::Callback(owner, event) => {
            (owner.invoke)(owner.context, &owner, event);
            finish_running(owner);
        }
    }
    true
}

fn finish_running(owner: FallibleArc<Core>) {
    let result = timers::with_callout_queue(owner.cpu, |queue, _| {
        // SAFETY: source queue ownership serializes callback completion with
        // reset, cancellation, shutdown, and waiting-thread registration.
        let state = unsafe { &mut *owner.state.get() };
        let old = core::mem::replace(&mut state.phase, Phase::Transition);
        let Phase::Running { node, next, .. } = old else {
            invariant("callout completed outside running state");
        };
        state.phase = match next {
            Some(next) if !state.closed => {
                state.execution = Some(owner.clone());
                owner.schedule(queue, node, next)
            }
            _ => Phase::Idle(node),
        };
    });
    if result.is_err() {
        invariant("callout completion source vanished");
    }
    // The owned pin keeps Core/WaitQueue alive even if another CPU observes
    // Idle and immediately destroys the public Callout and its typed context.
    wake_waiters(&owner);
}

fn wake_waiters(core: &Core) {
    if let Err(error) = core.waiters.wake_all() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: callout drain wake failed: {error:?}"
        ));
    }
}

fn invariant(message: &'static str) -> ! {
    hyper::debug::invariant_failure(message)
}
