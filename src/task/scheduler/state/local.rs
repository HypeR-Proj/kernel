// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-local scheduling, switch completion, and current-thread observations.
//!
//! These paths hold only the matching CPU lock. Transitions that need registry
//! coordination return a typed fallback before entering the coordinator.

use hyper::cpu::CpuIndex;

use super::super::queue::{self, CpuRunQueue};
use super::super::registry::{
    CpuScheduleAuthorityToken, CpuThreadTableAuthority, ThreadTableCapability,
};
use super::super::switch_handoff::{SwitchDisposition, SwitchHandoff};
use super::super::trace::{SchedulePoint, SwitchReason};
use super::super::{CrashTaskSnapshot, CurrentUser, CurrentVcpu, Error};
use super::{
    CPU_SCHEDULERS, CpuScheduler, LocalScheduleAttempt, LocalTailCompletion, PreparedContextSwitch,
    scheduler_invariant,
};
use crate::task::preempt;
use {crate::task::thread::ThreadId, crate::task::thread::ThreadState};

/// Both explicit yields and IRQ-tail preemption use the same policy decision.
pub(in crate::task::scheduler) fn prepare_local_yield(
    cpu: CpuIndex,
) -> Result<LocalScheduleAttempt, Error> {
    prepare_local_decision(cpu, true, SchedulePoint::Explicit)
}

pub(in crate::task::scheduler) fn prepare_local_preemption(
    cpu: CpuIndex,
    point: SchedulePoint,
) -> Result<LocalScheduleAttempt, Error> {
    prepare_local_decision(cpu, false, point)
}

fn prepare_local_decision(
    cpu: CpuIndex,
    yielding: bool,
    point: SchedulePoint,
) -> Result<LocalScheduleAttempt, Error> {
    super::runtime::with_accounted_cpu(cpu, |local| {
        if local.handoff.current().is_some() {
            return Err(Error::ThreadTransitionInProgress);
        }
        if !yielding && !preempt::pending(cpu)? {
            return Ok(LocalScheduleAttempt::Complete(None));
        }
        let current = local.current;
        if local
            .thread_authority()
            .with_thread(current, |_thread, schedule| {
                schedule.pending_migration.is_some()
            })?
        {
            return Ok(LocalScheduleAttempt::NeedsCoordinator);
        }
        let _ = preempt::take_pending_locked(cpu)?;
        let Some((next, front)) = local.choose_ready(yielding)? else {
            return Ok(LocalScheduleAttempt::Complete(None));
        };
        local.local_take_ready(next)?;
        let running = local
            .thread_authority()
            .with_thread(current, |_thread, schedule| {
                schedule.state == ThreadState::Running
            })?;
        let reason = if !running {
            SwitchReason::Idle
        } else if yielding {
            SwitchReason::Yield
        } else {
            SwitchReason::Preempt
        };
        if running {
            local.local_enqueue_ready(current, front)?;
        }
        let switch = local
            .prepare_local_switch(current, next, reason, point)
            .unwrap_or_else(|error| scheduler_invariant(error));
        Ok(LocalScheduleAttempt::Complete(Some(switch)))
    })
}

/// Completes an ordinary switch without entering the transition coordinator.
pub(in crate::task::scheduler) fn complete_local_switch_tail(
    cpu: CpuIndex,
    ticket: u64,
) -> Result<LocalTailCompletion, Error> {
    super::runtime::with_accounted_cpu(cpu, |local| {
        let switching = local
            .handoff
            .for_ticket(ticket)
            .ok_or(Error::PreemptionInvariant)?;
        #[cfg(feature = "kernel-trace")]
        crate::diagnostics::trace::publish_actor(cpu, local.current.get());
        if switching.disposition != SwitchDisposition::Local {
            return Ok(LocalTailCompletion::NeedsCoordinator);
        }
        let (state, pending) = {
            let threads = local.thread_authority();
            threads.with_thread(switching.thread, |_thread, schedule| {
                (schedule.state, schedule.pending_migration)
            })?
        };
        if pending.is_some()
            || !matches!(
                state,
                ThreadState::Ready | ThreadState::Idle | ThreadState::Blocked
            )
        {
            return Ok(LocalTailCompletion::NeedsCoordinator);
        }
        local
            .handoff
            .complete(ticket)
            .ok_or(Error::PreemptionInvariant)?;
        Ok(LocalTailCompletion::Complete)
    })
}

/// Best-effort crash observation taken under one non-blocking CPU lock.
///
/// Current identity, schedule state, and immutable resource metadata come
/// from one coherent CPU-domain snapshot. Failure to acquire that lock is
/// reported as absence; crash handling must never wait for scheduler state.
pub(in crate::task::scheduler) fn try_cpu_snapshot(cpu: CpuIndex) -> Option<CrashTaskSnapshot> {
    CPU_SCHEDULERS[cpu]
        .try_with(|slot| {
            let local = slot.as_mut()?;
            let current = local.current;
            let threads = local.thread_authority();
            threads
                .with_thread(current, |thread, schedule| CrashTaskSnapshot {
                    id: thread.id(),
                    state: schedule.state,
                    execution: thread.execution_kind(),
                    stack: thread.kernel_stack_bounds(),
                    // Current stack memory is live and cannot be scanned.
                    stack_statistics: None,
                })
                .ok()
        })
        .flatten()
}

pub(in crate::task::scheduler) fn local_current_thread(cpu: CpuIndex) -> Result<ThreadId, Error> {
    CPU_SCHEDULERS[cpu].with(|slot| {
        slot.as_ref()
            .map(|local| local.current)
            .ok_or(Error::CpuNotRegistered)
    })
}

pub(in crate::task::scheduler) fn local_current_vcpu(
    cpu: CpuIndex,
) -> Result<Option<CurrentVcpu>, Error> {
    CPU_SCHEDULERS[cpu].with(|slot| {
        let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
        let current = local.current;
        let threads = local.thread_authority();
        threads.with_thread(current, |thread, schedule| {
            if !matches!(schedule.state, ThreadState::Running | ThreadState::Idle) {
                return Err(Error::InvalidThreadState);
            }
            let Some(execution) = thread.vcpu_execution_pointer() else {
                return Ok(None);
            };
            let stack = thread.kernel_stack_bounds().ok_or(Error::Allocation)?;
            Ok(Some(CurrentVcpu {
                thread: current,
                execution,
                stack,
            }))
        })?
    })
}

pub(in crate::task::scheduler) fn local_current_user_thread(
    cpu: CpuIndex,
) -> Result<Option<crate::process::UserThread>, Error> {
    CPU_SCHEDULERS[cpu].with(|slot| {
        let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
        let id = local.current;
        local
            .thread_authority()
            .with_thread(id, |thread, schedule| {
                if schedule.state != ThreadState::Running {
                    return Err(Error::InvalidThreadState);
                }
                Ok(thread.user_thread().cloned())
            })?
    })
}

pub(in crate::task::scheduler) fn local_current_user(cpu: CpuIndex) -> Result<CurrentUser, Error> {
    CPU_SCHEDULERS[cpu].with(|slot| {
        let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
        let id = local.current;
        local
            .thread_authority()
            .with_thread(id, |thread, schedule| {
                if schedule.state != ThreadState::Running {
                    return Err(Error::InvalidThreadState);
                }
                Ok(CurrentUser {
                    thread: id,
                    object: thread
                        .user_thread()
                        .cloned()
                        .ok_or(Error::InvalidThreadState)?,
                    execution: thread
                        .user_execution_pointer()
                        .ok_or(Error::InvalidThreadState)?,
                    stack: thread.kernel_stack_bounds().ok_or(Error::Allocation)?,
                })
            })?
    })
}

impl CpuScheduler {
    pub(super) fn new(
        index: CpuIndex,
        table: ThreadTableCapability,
        current: ThreadId,
        scratch: queue::QueueScratch,
    ) -> Self {
        Self {
            index,
            table,
            authority: CpuScheduleAuthorityToken::new(),
            current,
            idle: None,
            run_queue: CpuRunQueue::new(scratch),
            handoff: SwitchHandoff::new(),
            last_account_ns: crate::time::monotonic_nanoseconds().ok(),
            window: super::super::algorithms::arbitration::CpuWindow::new(
                super::super::configuration::arbitration(),
            ),
            dispatch_mode: super::super::algorithms::arbitration::DispatchMode::Ordinary,
            deadline_generation: 0,
            armed_deadline_ns: None,
            fair_floor: 0,
        }
    }

    pub(super) fn thread_authority(&mut self) -> CpuThreadTableAuthority<'_> {
        self.table.cpu_authority(self.index, &mut self.authority)
    }

    // Keep selector scratch out of callers that can run deep in a syscall or
    // IRQ-tail chain. The emitted frame is bounded by the image stack audit.
    #[inline(never)]
    pub(super) fn local_selection(
        &mut self,
        current: Option<super::super::algorithms::arbitration::Candidate>,
    ) -> Result<queue::Selection, Error> {
        let cpu = self.index;
        let now = self.now();
        let mut threads = queue::LocalReadyQueueAuthority::new(
            self.table.cpu_authority(cpu, &mut self.authority),
        );
        let selection =
            self.run_queue
                .peek_next(&mut threads, cpu, now, &mut self.window, current)?;
        if let Some(floor) = selection.fair_min_pass {
            self.fair_floor = self.fair_floor.max(floor);
        }
        Ok(selection)
    }

    pub(super) fn local_peek_ready(&mut self) -> Result<Option<queue::ReadyThread>, Error> {
        self.local_selection(None).map(|selection| selection.ready)
    }

    pub(super) fn local_take_ready(&mut self, id: ThreadId) -> Result<(), Error> {
        let cpu = self.index;
        let membership = self
            .thread_authority()
            .with_thread(id, |_thread, schedule| {
                schedule.ready_queue_links.membership
            })?;
        let mut threads = queue::LocalReadyQueueAuthority::new(
            self.table.cpu_authority(cpu, &mut self.authority),
        );
        self.run_queue.remove(&mut threads, id, cpu, membership)
    }

    pub(super) fn local_enqueue_ready(&mut self, id: ThreadId, front: bool) -> Result<(), Error> {
        let cpu = self.index;
        let floor = self.fair_floor;
        let now = self.now();
        self.thread_authority()
            .with_thread_mut(id, |_thread, schedule| {
                super::runtime::prepare_enqueue(schedule, floor, front, now);
            })?;
        let mut threads = queue::LocalReadyQueueAuthority::new(
            self.table.cpu_authority(cpu, &mut self.authority),
        );
        if front {
            self.run_queue.enqueue_front(&mut threads, id, cpu)
        } else {
            self.run_queue.enqueue(&mut threads, id, cpu)
        }
    }

    pub(super) fn local_dequeue_ready(&mut self) -> Result<Option<ThreadId>, Error> {
        let Some(candidate) = self.local_peek_ready()? else {
            return Ok(None);
        };
        self.local_take_ready(candidate.id)?;
        Ok(Some(candidate.id))
    }

    pub(super) fn prepare_local_switch(
        &mut self,
        current: ThreadId,
        next: ThreadId,
        _reason: SwitchReason,
        _point: SchedulePoint,
    ) -> Result<PreparedContextSwitch, Error> {
        if self.handoff.current().is_some() || self.current != current || current == next {
            return Err(Error::ThreadTransitionInProgress);
        }
        let cpu = self.index;
        let (previous, next_context) = {
            let mut threads = self.thread_authority();
            let previous =
                threads.with_thread(current, |thread, _schedule| thread.context_pointer())?;
            let next_context = threads.with_thread_mut(next, |thread, schedule| {
                match schedule.state {
                    ThreadState::Ready => {
                        let Some(placement) = schedule.placement.mark_running(cpu) else {
                            return Err(Error::InvalidThreadState);
                        };
                        schedule.placement = placement;
                        schedule.state = ThreadState::Running;
                        schedule.auto_migrated = false;
                    }
                    ThreadState::Idle => super::super::balance::request(),
                    _ => return Err(Error::InvalidThreadState),
                }
                if schedule.fair_slice_expired() {
                    schedule.replenish_fair_slice();
                }
                Ok::<_, Error>(thread.context_pointer().cast_const())
            })??;
            (previous, next_context)
        };
        let generation = self
            .handoff
            .begin(current, SwitchDisposition::Local)
            .unwrap_or_else(|| {
                hyper::debug::invariant_failure(
                    "task::scheduler::state::prepare_local_switch invariant",
                )
            });
        crate::tracepoint!(
            Scheduler,
            "sched_switch",
            prev = current.get(),
            next = next.get(),
            state = super::super::trace::switch_state(
                self.thread_authority()
                    .with_thread(current, |_thread, schedule| schedule.state as u64)
                    .unwrap_or(0),
                self.thread_authority()
                    .with_thread(next, |_thread, schedule| schedule.scheduling.class() as u64)
                    .unwrap_or(0)
            ),
            cause = super::super::trace::switch_cause(_reason, _point)
        );
        self.current = next;
        Ok(PreparedContextSwitch {
            previous,
            next: next_context,
            ticket: generation,
            armed: true,
        })
    }
}
