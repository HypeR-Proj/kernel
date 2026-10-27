// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reusable IRQ-context callouts with exact-generation retirement.
//!
//! Notifications execute in hard IRQ context without the deadline queue lock.
//! They must be bounded and must not allocate or block. Retirement joins the
//! exact invocation and requires no locks which that invocation could acquire.

use hyper::cpu::CpuIndex;
use hyper::sync::InterruptSpinLock;
use {
    crate::time::queue::ReservedTimerCallbacks, crate::time::queue::ReservedTimerNode,
    crate::time::queue::TimerEvent, crate::time::queue::TimerHandle,
    crate::time::queue::TimerQueueError,
};

use super::super::{Error, monotonic_ticks};
use super::{
    TIMERS, current_cpu, ensure_initialized, pin_current_timer_cpu, program_next,
    release_timer_cpu_pin,
};

type ReservationLock = InterruptSpinLock<ReservationState, crate::hal::irq::LocalMask>;

enum ReservationState {
    Idle {
        node: Option<ReservedTimerNode>,
        completed_generation: u64,
    },
    Arming {
        generation: u64,
    },
    Queued {
        generation: u64,
        handle: TimerHandle,
        notify: fn(usize),
        notify_context: usize,
    },
    Firing {
        generation: u64,
        handle: TimerHandle,
        notify: fn(usize),
        notify_context: usize,
    },
    Completed {
        generation: u64,
        handle: TimerHandle,
        node: Option<ReservedTimerNode>,
    },
}

/// A single preallocated one-shot timer which can be rearmed without allocation.
pub(crate) struct IrqCallout {
    state: ReservationLock,
}

impl IrqCallout {
    pub(crate) const fn allocation_size() -> usize {
        ReservedTimerNode::allocation_size()
    }

    pub(crate) fn try_new() -> Result<Self, Error> {
        let node = ReservedTimerNode::try_new()?;
        Ok(Self {
            state: ReservationLock::new(ReservationState::Idle {
                node: Some(node),
                completed_generation: 0,
            }),
        })
    }

    pub(crate) fn arm(
        &self,
        deadline: u64,
        notify: fn(usize),
        notify_context: usize,
    ) -> Result<ArmedIrqCallout<'_>, Error> {
        let (cpu_pin, cpu) = pin_current_timer_cpu()?;
        let result = self.arm_on_pinned_cpu(cpu, deadline, notify, notify_context);
        release_timer_cpu_pin(cpu_pin);
        result
    }

    /// Own the stable reservation allocation until this exact arm is retired.
    #[allow(
        dead_code,
        reason = "selected device backends may use borrowed reservations only"
    )]
    pub(crate) fn arm_owned(
        reservation: hyper::mm::FallibleArc<Self>,
        deadline: u64,
        notify: fn(usize),
        notify_context: usize,
    ) -> Result<OwnedArmedIrqCallout, Error> {
        let (handle, generation) = {
            let mut borrowed = reservation.arm(deadline, notify, notify_context)?;
            let identity = (borrowed.handle, borrowed.generation);
            // Transfer the linear obligation, never the callback or queue node.
            borrowed.armed = false;
            identity
        };
        Ok(OwnedArmedIrqCallout {
            reservation,
            handle,
            generation,
            armed: true,
        })
    }

    fn arm_on_pinned_cpu(
        &self,
        cpu: CpuIndex,
        deadline: u64,
        notify: fn(usize),
        notify_context: usize,
    ) -> Result<ArmedIrqCallout<'_>, Error> {
        TIMERS[cpu].with(|timers| ensure_initialized(timers))?;
        let (node, generation) = self.state.with(|state| {
            let ReservationState::Idle {
                node,
                completed_generation,
            } = state
            else {
                return Err(Error::TimerQueue(TimerQueueError::InvalidHandle));
            };
            let generation = completed_generation
                .checked_add(1)
                .ok_or(Error::TimerQueue(TimerQueueError::InvalidHandle))?;
            let node = node
                .take()
                .ok_or(Error::TimerQueue(TimerQueueError::InvalidHandle))?;
            *state = ReservationState::Arming { generation };
            Ok((node, generation))
        })?;
        let context = core::ptr::from_ref(self).expose_provenance();
        let pending = node.prepare(
            deadline,
            ReservedTimerCallbacks {
                callback: reserved_expire,
                context,
                claim: claim_reserved,
                claim_context: context,
                recycle: recycle_reserved,
                recycle_context: context,
            },
        );
        TIMERS[cpu].with(|timers| {
            let previous = timers.queue.next_deadline();
            let handle = timers.queue.insert_reserved(pending, monotonic_ticks());
            self.state.with(|state| match state {
                ReservationState::Arming {
                    generation: pending_generation,
                } if *pending_generation == generation => {
                    *state = ReservationState::Queued {
                        generation,
                        handle,
                        notify,
                        notify_context,
                    };
                }
                _ => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reserved timer lost its arming ownership"
                )),
            });
            if timers.queue.next_deadline() != previous
                && let Err(error) = program_next(timers)
            {
                let node = match timers.queue.cancel_reserved(handle) {
                    Ok(node) => node,
                    Err(_) => crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: reserved timer arm rollback failed"
                    )),
                };
                self.restore_cancelled(generation, handle, node);
                return Err(error);
            }
            Ok(ArmedIrqCallout {
                reservation: self,
                handle,
                generation,
                armed: true,
            })
        })
    }

    fn restore_cancelled(&self, generation: u64, handle: TimerHandle, node: ReservedTimerNode) {
        self.state.with(|state| match state {
            ReservationState::Queued {
                generation: current,
                handle: current_handle,
                ..
            } if *current == generation && *current_handle == handle => {
                *state = ReservationState::Idle {
                    node: Some(node),
                    completed_generation: generation,
                };
            }
            _ => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: reserved timer cancellation ownership mismatch"
            )),
        });
    }

    fn claim_callback(&self, event: TimerEvent) {
        self.state.with(|state| match state {
            ReservationState::Queued {
                generation,
                handle,
                notify,
                notify_context,
            } if *handle == event.handle => {
                *state = ReservationState::Firing {
                    generation: *generation,
                    handle: *handle,
                    notify: *notify,
                    notify_context: *notify_context,
                };
            }
            _ => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: stale reserved timer callback"
            )),
        })
    }

    fn notify_callback(&self, event: TimerEvent) {
        let (notify, context) = self.state.with(|state| match state {
            ReservationState::Firing {
                handle,
                notify,
                notify_context,
                ..
            } if *handle == event.handle => (*notify, *notify_context),
            _ => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: reserved timer callback lost its firing ownership"
            )),
        });
        notify(context);
    }

    fn recycle(&self, node: ReservedTimerNode) {
        self.state.with(|state| {
            let ReservationState::Firing {
                generation, handle, ..
            } = state
            else {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reserved timer recycled outside callback ownership"
                ));
            };
            let generation = *generation;
            let handle = *handle;
            // The callback has completed, but its linear ArmedIrqCallout
            // still owns this generation. Only exact retirement may return
            // the node to Idle and permit rearming.
            *state = ReservationState::Completed {
                generation,
                handle,
                node: Some(node),
            };
        });
    }

    fn retire(&self, handle: TimerHandle, generation: u64) -> Result<(), Error> {
        let owner = CpuIndex::new(handle.queue_id()).ok_or(Error::InvalidCpuIndex)?;
        let cancelled = TIMERS[owner].with(|timers| {
            ensure_initialized(timers)?;
            let previous = timers.queue.next_deadline();
            match timers.queue.cancel_reserved(handle) {
                Ok(node) => {
                    self.restore_cancelled(generation, handle, node);
                    // This CPU classification is made while the queue's
                    // InterruptSpinLock masks local IRQs, so IRQ-tail
                    // migration cannot intervene before comparator update.
                    if owner == current_cpu()? && timers.queue.next_deadline() != previous {
                        program_next(timers)?;
                    }
                    Ok::<bool, Error>(true)
                }
                Err(TimerQueueError::InvalidHandle) => Ok::<bool, Error>(false),
                Err(error) => Err(error.into()),
            }
        })?;
        if cancelled {
            return Ok(());
        }
        loop {
            let complete = self.state.with(|state| match state {
                ReservationState::Firing {
                    generation: current,
                    handle: current_handle,
                    ..
                } if *current == generation && *current_handle == handle => false,
                ReservationState::Completed {
                    generation: current,
                    handle: current_handle,
                    node,
                } if *current == generation && *current_handle == handle => {
                    let node = match node.take() {
                        Some(node) => node,
                        None => crate::diagnostics::crash::fatal(format_args!(
                            "HypeR: completed reserved timer lost its node"
                        )),
                    };
                    *state = ReservationState::Idle {
                        node: Some(node),
                        completed_generation: generation,
                    };
                    true
                }
                _ => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: reserved timer vanished without exact callback ownership"
                )),
            });
            if complete {
                return Ok(());
            }
            core::hint::spin_loop();
        }
    }
}

impl Drop for IrqCallout {
    fn drop(&mut self) {
        let idle = self
            .state
            .with(|state| matches!(state, ReservationState::Idle { node: Some(_), .. }));
        if !idle {
            hyper::debug::invariant_failure("time::timers::drop invariant")
        }
    }
}

#[must_use = "an armed reserved timer must be retired exactly once"]
pub(crate) struct ArmedIrqCallout<'a> {
    reservation: &'a IrqCallout,
    handle: TimerHandle,
    generation: u64,
    armed: bool,
}

impl ArmedIrqCallout<'_> {
    pub(crate) fn retire(mut self) -> Result<(), Error> {
        self.reservation.retire(self.handle, self.generation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for ArmedIrqCallout<'_> {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("time::timers::drop invariant")
        }
    }
}

/// A movable owner of one exact reserved arm. No borrowed owner address escapes.
#[allow(
    dead_code,
    reason = "selected device backends may use borrowed reservations only"
)]
#[must_use = "an armed reserved timer must be retired exactly once"]
pub(crate) struct OwnedArmedIrqCallout {
    reservation: hyper::mm::FallibleArc<IrqCallout>,
    handle: TimerHandle,
    generation: u64,
    armed: bool,
}

#[allow(
    dead_code,
    reason = "selected device backends may use borrowed reservations only"
)]
impl OwnedArmedIrqCallout {
    pub(crate) fn retire(mut self) -> Result<(), Error> {
        self.reservation.retire(self.handle, self.generation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for OwnedArmedIrqCallout {
    fn drop(&mut self) {
        if self.armed {
            hyper::debug::invariant_failure("time::timers::drop invariant")
        }
    }
}

fn reserved_expire(event: TimerEvent, context: usize) {
    let pointer = core::ptr::with_exposed_provenance::<IrqCallout>(context);
    // SAFETY: an armed reserved node borrows its stable reservation until the
    // callback is recycled or exact cancellation returns the node.
    let reservation = unsafe { &*pointer };
    reservation.notify_callback(event);
}

fn claim_reserved(event: TimerEvent, context: usize) {
    let pointer = core::ptr::with_exposed_provenance::<IrqCallout>(context);
    // SAFETY: queue ownership retains this stable reservation while claiming
    // expiry under the queue lock.
    let reservation = unsafe { &*pointer };
    reservation.claim_callback(event);
}

fn recycle_reserved(node: ReservedTimerNode, context: usize) {
    let pointer = core::ptr::with_exposed_provenance::<IrqCallout>(context);
    // SAFETY: the same reserved node carries the reservation pointer installed
    // by `arm`, and recycling is invoked exactly once after its callback.
    let reservation = unsafe { &*pointer };
    reservation.recycle(node);
}
