// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Thread affinity and migration from source context release to target publication.
//!
//! Running contexts remain on their source CPU until the incoming switch tail
//! acknowledges the save. Ready and blocked transfers retain their distinct
//! queue ownership and deferred publication protocols.

use hyper::cpu::CpuIndex;

use super::super::{Error, MigrationStatus};
use super::{
    MigrationOutcome, PreparedContextSwitch, ReadyOutcome, Scheduler, scheduler_invariant,
};
use {crate::task::policy::CpuMask, crate::task::policy::PlacementPolicy};
use {
    crate::task::thread::ExecutionKind, crate::task::thread::MigrationRequest,
    crate::task::thread::QueueMembership, crate::task::thread::Thread,
    crate::task::thread::ThreadId, crate::task::thread::ThreadState,
};

impl Scheduler {
    fn record_fair_origin(&mut self, id: ThreadId) -> Result<(), Error> {
        if let Some(active) = self.active_domain {
            // SAFETY: this source CPU domain is held by the migration transaction.
            let floor = unsafe { active.local.as_ref() }.fair_floor;
            self.thread_mut(id)?.apply_schedule(|schedule| {
                if schedule.fair_origin.is_none() {
                    schedule.fair_origin = Some(floor);
                }
            });
        }
        Ok(())
    }

    pub fn migrate_thread(
        &mut self,
        id: ThreadId,
        target: CpuIndex,
    ) -> Result<MigrationOutcome, Error> {
        self.schedulable_cpu_slot(target)?;
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self
                .with_cpu_schedule_stored(cpu, |scheduler| scheduler.migrate_thread(id, target));
        }
        let thread = self.thread(id)?;
        if !thread.can_run_on(target) {
            return Err(Error::CpuNotAllowed);
        }
        let plan = MigrationRequest {
            target,
            affinity: thread.affinity(),
        };
        self.request_migration(id, plan)
    }

    pub fn set_thread_affinity(
        &mut self,
        id: ThreadId,
        affinity: CpuMask,
    ) -> Result<MigrationOutcome, Error> {
        if affinity.is_empty() {
            return Err(Error::EmptyCpuAffinity);
        }
        if let Some(cpu) = self.cpu_lock_required_for(id)? {
            return self.with_cpu_schedule_stored(cpu, |scheduler| {
                scheduler.set_thread_affinity(id, affinity)
            });
        }
        let assigned = self.thread(id)?.cpu_index();
        let target = if affinity.contains(assigned) {
            assigned
        } else {
            self.select_cpu(assigned, affinity)?
        };
        self.request_migration(id, MigrationRequest { target, affinity })
    }

    /// Changes placement only after proving that no CPU can still use stale context.
    fn request_migration(
        &mut self,
        id: ThreadId,
        plan: MigrationRequest,
    ) -> Result<MigrationOutcome, Error> {
        let thread = self.thread(id)?;
        if !matches!(
            thread.execution_kind(),
            ExecutionKind::Kernel | ExecutionKind::User | ExecutionKind::Vcpu
        ) || thread.placement_policy() != PlacementPolicy::Movable
        {
            return Err(Error::MigrationUnsupported);
        }
        if !plan.affinity.contains(plan.target) || !self.cpu_is_schedulable(plan.target) {
            return Err(Error::NoRegisteredCpuInAffinity);
        }
        if !thread.wait_record().permits_assignment(plan.target) {
            return Err(Error::MigrationBlockedByCpuLocalWait);
        }
        if thread.state() == ThreadState::Terminated {
            return Err(Error::TerminatedThread);
        }

        let state = self.thread(id)?.state();
        let source = self.thread(id)?.cpu_index();
        if let Some(existing) = self.thread(id)?.pending_migration() {
            if existing != plan {
                return Err(Error::MigrationInProgress);
            }
            return Ok(MigrationOutcome {
                status: MigrationStatus::Pending,
                source_reschedule: (state == ThreadState::Running).then_some(source),
                target_ready: None,
            });
        }
        // An affinity-only update retains assignment and cannot expose the
        // outgoing context on another CPU. Applying it in place also preserves
        // the Thread's exact ready-queue position.
        if source == plan.target {
            if !self.thread_mut(id)?.replace_affinity(plan.affinity) {
                return Err(Error::InvalidThreadState);
            }
            return Ok(MigrationOutcome {
                status: MigrationStatus::Completed,
                source_reschedule: None,
                target_ready: None,
            });
        }

        // The scheduler transaction precedes the assembly context save. Any
        // state reached through `switching_from` is therefore still source-CPU
        // owned even if a concurrent wake has already made it Ready. Retain the
        // request on that exact Thread for its incoming tail to consume.
        if self
            .switching_from(source)?
            .is_some_and(|switching| switching.thread == id)
        {
            if !self.thread_mut(id)?.request_migration(plan) {
                return Err(Error::MigrationInProgress);
            }
            return Ok(MigrationOutcome {
                status: MigrationStatus::Pending,
                source_reschedule: None,
                target_ready: None,
            });
        }

        match state {
            ThreadState::Dormant | ThreadState::Blocked => {
                if state == ThreadState::Blocked
                    && !matches!(
                        self.thread(id)?.queue_links().membership,
                        QueueMembership::Waiting { .. }
                    )
                {
                    return Err(Error::QueueCorrupted);
                }
                if state == ThreadState::Blocked {
                    self.move_blocked_thread(id, plan)?;
                } else if !self
                    .thread_mut(id)?
                    .reassign_stopped_with_affinity(plan.target, plan.affinity)
                {
                    return Err(Error::InvalidThreadState);
                }
                Ok(MigrationOutcome {
                    status: MigrationStatus::Completed,
                    source_reschedule: None,
                    target_ready: None,
                })
            }
            ThreadState::Ready => {
                let ready = self.move_ready_thread(id, plan)?;
                Ok(MigrationOutcome {
                    status: MigrationStatus::Completed,
                    source_reschedule: None,
                    target_ready: Some(ready),
                })
            }
            ThreadState::Running => {
                let cpu_slot = self.cpu_slot(source)?;
                if self.current_thread(cpu_slot)? != id {
                    return Err(Error::InvalidThreadState);
                }
                if !self.thread_mut(id)?.request_migration(plan) {
                    return Err(Error::MigrationInProgress);
                }
                Ok(MigrationOutcome {
                    status: MigrationStatus::Pending,
                    source_reschedule: Some(source),
                    target_ready: None,
                })
            }
            ThreadState::Migrating => Err(Error::MigrationInProgress),
            ThreadState::Idle => Err(Error::MigrationUnsupported),
            ThreadState::Terminated => Err(Error::TerminatedThread),
        }
    }

    pub(super) fn prepare_running_migration(
        &mut self,
        cpu_slot: CpuIndex,
        current: ThreadId,
        point: super::SchedulePoint,
    ) -> Result<PreparedContextSwitch, Error> {
        if self.thread(current)?.state() != ThreadState::Running
            || self.thread(current)?.pending_migration().is_none()
        {
            return Err(Error::InvalidThreadState);
        }
        let next = self
            .dequeue_ready(cpu_slot)?
            .or(self.idle_thread(cpu_slot)?)
            .ok_or(Error::CurrentThreadMissing)?;
        if next == current {
            return Err(Error::InvalidThreadState);
        }
        self.record_fair_origin(current)?;
        self.thread_mut(current)?.set_state(ThreadState::Migrating);
        self.prepare_switch(cpu_slot, current, next, super::SwitchReason::Migrate, point)
    }

    fn move_blocked_thread(&mut self, id: ThreadId, plan: MigrationRequest) -> Result<(), Error> {
        self.record_fair_origin(id)?;
        if let Some(source) = self.registry.with_thread(id, Thread::schedule_owner_cpu)?
            && !self
                .registry
                .with_thread_mut(id, |thread| thread.release_schedule(source))?
        {
            scheduler_invariant(Error::InvalidThreadState);
        }
        if !self
            .thread_mut(id)?
            .reassign_stopped_with_affinity(plan.target, plan.affinity)
        {
            scheduler_invariant(Error::InvalidThreadState);
        }
        if self.active_domain.is_some() {
            if self
                .deferred_blocked_handoff
                .replace((id, plan.target))
                .is_some()
            {
                scheduler_invariant(Error::InvalidThreadState);
            }
        } else {
            self.publish_blocked_handoff(id, plan.target)?;
        }
        Ok(())
    }

    pub(super) fn publish_blocked_handoff(
        &mut self,
        id: ThreadId,
        target: CpuIndex,
    ) -> Result<(), Error> {
        self.with_cpu_domain(target, |scheduler, _local| {
            if !scheduler
                .registry
                .with_thread_mut(id, |thread| thread.claim_schedule(target))?
            {
                scheduler_invariant(Error::InvalidThreadState);
            }
            Ok(())
        })
    }

    pub(super) fn complete_migration(
        &mut self,
        id: ThreadId,
        plan: MigrationRequest,
    ) -> Result<Option<ReadyOutcome>, Error> {
        match self.thread(id)?.state() {
            ThreadState::Ready => self.move_ready_thread(id, plan).map(Some),
            ThreadState::Blocked => {
                if !matches!(
                    self.thread(id)?.queue_links().membership,
                    QueueMembership::Waiting { .. }
                ) {
                    return Err(Error::QueueCorrupted);
                }
                self.move_blocked_thread(id, plan)?;
                Ok(None)
            }
            ThreadState::Migrating => {
                if self.thread(id)?.queue_links().membership != QueueMembership::None
                    || !self
                        .thread_mut(id)?
                        .reassign_stopped_with_affinity(plan.target, plan.affinity)
                {
                    return Err(Error::InvalidThreadState);
                }
                self.enqueue_ready_or_defer(id, plan.target)?;
                crate::tracepoint!(
                    Scheduler,
                    "sched_migrate",
                    thread = id.get(),
                    target_cpu = plan.target.get() as u64
                );
                Ok(Some(ReadyOutcome {
                    changed: true,
                    target_cpu: plan.target,
                }))
            }
            // Exit won the race with a remote migration publication. The
            // Thread will be reclaimed after this same tail releases source
            // ownership, so no target assignment is published.
            ThreadState::Terminated => Ok(None),
            _ => Err(Error::InvalidThreadState),
        }
    }

    fn move_ready_thread(
        &mut self,
        id: ThreadId,
        plan: MigrationRequest,
    ) -> Result<ReadyOutcome, Error> {
        self.record_fair_origin(id)?;
        let source = self.thread(id)?.cpu_index();
        let old_affinity = self.thread(id)?.affinity();
        let membership = self.thread(id)?.queue_links().membership;
        if !matches!(
            membership,
            QueueMembership::ReadyRealTime { cpu, .. } | QueueMembership::ReadyFair { cpu }
                if cpu == source
        ) {
            return Err(Error::QueueCorrupted);
        }
        self.remove_ready(id, source, membership)?;
        if !self
            .registry
            .with_thread_mut(id, |thread| thread.release_schedule(source))?
        {
            scheduler_invariant(Error::InvalidThreadState);
        }
        if !self
            .thread_mut(id)?
            .reassign_stopped_with_affinity(plan.target, plan.affinity)
        {
            self.restore_ready_migration(id, source, old_affinity);
            return Err(Error::InvalidThreadState);
        }
        self.enqueue_ready_or_defer(id, plan.target)?;
        crate::tracepoint!(
            Scheduler,
            "sched_migrate",
            thread = id.get(),
            target_cpu = plan.target.get() as u64
        );
        Ok(ReadyOutcome {
            changed: true,
            target_cpu: plan.target,
        })
    }

    fn restore_ready_migration(&mut self, id: ThreadId, cpu: CpuIndex, affinity: CpuMask) {
        let restored = self
            .thread_mut(id)
            .and_then(|mut thread| {
                thread
                    .reassign_stopped_with_affinity(cpu, affinity)
                    .then_some(())
                    .ok_or(Error::InvalidThreadState)
            })
            .and_then(|()| self.enqueue_ready(id));
        if restored.is_err() {
            // The global scheduler lock is held and queue state is no longer
            // recoverable. Use crash-safe reporting, which never waits for
            // scheduler locks, and retain all inconsistent ownership.
            hyper::debug::invariant_failure(
                "task::scheduler::state::restore_ready_migration invariant",
            )
        }
    }
}
