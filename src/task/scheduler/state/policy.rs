// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduling-class and priority changes under existing queue ownership.

use hyper::cpu::CpuIndex;

use super::super::Error;
use super::Scheduler;
use {crate::task::policy::SchedulingPolicy, crate::task::policy::ThreadPriority};
use {
    crate::task::thread::DeferredFifoPlacement, crate::task::thread::QueueMembership,
    crate::task::thread::ThreadId, crate::task::thread::ThreadState,
};

impl Scheduler {
    /// Restore Fair history in this CPU's virtual-time domain before it can
    /// compete, including policy changes on a currently running Thread.
    fn admit_fair_history(&mut self, id: ThreadId) -> Result<(), Error> {
        let floor = self.active_domain.map(|active| {
            // SAFETY: the enclosing configuration transaction owns this CPU lock.
            unsafe { active.local.as_ref() }.fair_floor
        });
        self.thread_mut(id)?.apply_schedule(|schedule| {
            if let (Some(floor), super::super::algorithms::PolicyRuntime::Fair(runtime)) =
                (floor, &mut schedule.runtime)
            {
                if let Some(origin) = schedule.fair_origin.take() {
                    runtime.migrate_floor(origin, floor);
                }
                runtime.admit(floor);
            }
        });
        Ok(())
    }

    pub fn scheduling_snapshot(
        &mut self,
        id: ThreadId,
    ) -> Result<crate::task::policy::SchedulingParameters, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self
                .with_cpu_schedule_stored(cpu, |scheduler| scheduler.scheduling_snapshot(id));
        }
        Ok(self
            .thread_mut(id)?
            .apply_schedule(|schedule| schedule.parameters))
    }

    pub fn configure_scheduling(
        &mut self,
        id: ThreadId,
        parameters: crate::task::policy::SchedulingParameters,
        authority: crate::task::scheduling_authority::SchedulingAuthority,
        limits: crate::task::scheduling_authority::SchedulingLimits,
    ) -> Result<Option<CpuIndex>, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.configure_scheduling(id, parameters, authority, limits)
            });
        }
        let old = self.scheduling_snapshot(id)?;
        authority
            .validate_change(old, parameters, limits)
            .map_err(Error::SchedulingPermission)?;
        let thread = self.thread(id)?;
        if thread.state() == ThreadState::Terminated {
            return Err(Error::TerminatedThread);
        }
        if old.policy() == SchedulingPolicy::Idle {
            return Err(Error::InvalidThreadState);
        }
        if old == parameters {
            return Ok(None);
        }
        let cpu = thread.cpu_index();
        let membership = thread.queue_links().membership;
        let ready = matches!(
            membership,
            QueueMembership::ReadyFair { .. } | QueueMembership::ReadyRealTime { .. }
        );
        if ready {
            self.remove_ready(id, cpu, membership)
                .unwrap_or_else(|error| super::scheduler_invariant(error));
        }
        self.thread_mut(id)?
            .apply_schedule(|schedule| schedule.install_parameters(parameters));
        self.admit_fair_history(id)?;
        if ready {
            self.enqueue_ready(id)
                .unwrap_or_else(|error| super::scheduler_invariant(error));
        }
        Ok(matches!(thread.state(), ThreadState::Ready | ThreadState::Running).then_some(cpu))
    }

    pub fn set_fifo_policy(
        &mut self,
        id: ThreadId,
        priority: ThreadPriority,
    ) -> Result<Option<CpuIndex>, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.set_fifo_policy(id, priority)
            });
        }
        let state = self.thread(id)?.state();
        if state == ThreadState::Terminated {
            return Err(Error::TerminatedThread);
        }
        let links = self.thread(id)?.queue_links();
        match links.membership {
            QueueMembership::ReadyRealTime { cpu, priority: old } => {
                if old == priority.get() {
                    return Ok(None);
                }
                self.remove_ready(id, cpu, links.membership)?;
                if !self
                    .thread_mut(id)?
                    .set_scheduling_policy(SchedulingPolicy::fifo(priority))
                {
                    return Err(Error::InvalidThreadState);
                }
                if priority.get() < old {
                    self.enqueue_ready(id)?;
                } else {
                    self.enqueue_ready_front(id)?;
                }
                Ok(Some(cpu))
            }
            QueueMembership::ReadyFair { cpu } => {
                self.remove_ready(id, cpu, links.membership)?;
                if !self
                    .thread_mut(id)?
                    .set_scheduling_policy(SchedulingPolicy::fifo(priority))
                {
                    return Err(Error::InvalidThreadState);
                }
                self.enqueue_ready(id)?;
                Ok(Some(cpu))
            }
            QueueMembership::None
            | QueueMembership::Waiting { .. }
            | QueueMembership::Terminated => {
                let previous_policy = self.thread(id)?.scheduling_policy();
                let old = previous_policy.priority();
                if previous_policy == SchedulingPolicy::fifo(priority) {
                    return Ok(None);
                }
                if !self
                    .thread_mut(id)?
                    .set_scheduling_policy(SchedulingPolicy::fifo(priority))
                {
                    return Err(Error::InvalidThreadState);
                }
                if state != ThreadState::Running {
                    return Ok(None);
                }
                self.thread_mut(id)?.set_deferred_fifo_placement(None);
                let cpu = self.thread(id)?.cpu_index();
                let ready = self.peek_ready(cpu)?;
                let changes_fifo_position = ready.is_some_and(|ready| match old {
                    Some(old) => matches!(
                        ready.policy,
                        SchedulingPolicy::Fifo { priority: ready }
                            if ready < priority || (priority < old && ready == priority)
                    ),
                    None => false,
                });
                if changes_fifo_position && let Some(old) = old {
                    let placement = if priority < old {
                        DeferredFifoPlacement::Tail
                    } else {
                        DeferredFifoPlacement::Head
                    };
                    self.thread_mut(id)?
                        .set_deferred_fifo_placement(Some(placement));
                }
                // The arbiter, not a static class comparison, decides whether
                // this changed policy should yield to any runnable algorithm.
                Ok(Some(cpu))
            }
        }
    }

    /// Moves a non-idle thread into the Fair scheduling class.
    ///
    /// Ready membership is transferred between class queues while the global
    /// scheduler lock is held. Any runnable policy change prompts the CPU to
    /// reconsider arbitration and its one-shot deadline.
    pub fn set_fair_policy(&mut self, id: ThreadId) -> Result<Option<CpuIndex>, Error> {
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| scheduler.set_fair_policy(id));
        }
        let state = self.thread(id)?.state();
        if state == ThreadState::Terminated {
            return Err(Error::TerminatedThread);
        }
        if self.thread(id)?.scheduling_policy() == SchedulingPolicy::Fair {
            return Ok(None);
        }

        let membership = self.thread(id)?.queue_links().membership;
        if let QueueMembership::ReadyRealTime { cpu, .. } | QueueMembership::ReadyFair { cpu } =
            membership
        {
            self.remove_ready(id, cpu, membership)?;
            if !self
                .thread_mut(id)?
                .set_scheduling_policy(SchedulingPolicy::fair())
            {
                return Err(Error::InvalidThreadState);
            }
            self.enqueue_ready(id)?;
            return Ok(Some(cpu));
        }

        if !self
            .thread_mut(id)?
            .set_scheduling_policy(SchedulingPolicy::fair())
        {
            return Err(Error::InvalidThreadState);
        }
        self.admit_fair_history(id)?;
        if state != ThreadState::Running {
            return Ok(None);
        }

        Ok(Some(self.thread(id)?.cpu_index()))
    }
}
