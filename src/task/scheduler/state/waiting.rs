// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concurrent waits: registry reader lane -> owner CPU -> individual queue.
//!
//! Blocked schedules retain their CPU residence. The reader lane pins both
//! identity and residence, so remote resolution needs neither a registry scan
//! nor a global write lock. Queue-head discovery releases the queue lock before
//! taking a CPU lock and revalidates the head afterwards; locks never invert.

use super::*;

impl Scheduler {
    pub fn with_wait_queue<R>(
        &self,
        wait_queue: &WaitQueue,
        operation: impl FnOnce(&mut ThreadQueue) -> R,
    ) -> R {
        // SAFETY: shared Scheduler borrows are bounded by a registry reader
        // lane or exclusive coordination. The queue lock then serializes its
        // topology without excluding readers working on unrelated queues.
        unsafe { wait_queue.with_state(operation) }
    }

    pub fn wait_queue_snapshot(&self, wait_queue: &WaitQueue) -> Result<ThreadQueue, Error> {
        self.with_wait_queue(wait_queue, |queue| {
            match (queue.head, queue.tail, queue.len) {
                (None, None, 0) | (Some(_), Some(_), 1..) => Ok(*queue),
                _ => Err(Error::QueueCorrupted),
            }
        })
    }

    pub fn arm_wait_shared(
        &self,
        cpu: CpuIndex,
        mobility: WaitMobility,
    ) -> Result<WaitTicket, Error> {
        super::runtime::with_accounted_cpu(cpu, |local| {
            let current = local.current;
            local
                .thread_authority()
                .with_thread_mut(current, |thread, schedule| {
                    if !thread.wait_context().is_idle() {
                        return Err(Error::InvalidWaitRegistration);
                    }
                    if schedule.state != ThreadState::Running {
                        return Err(Error::CannotBlockIdle);
                    }
                    if mobility == WaitMobility::CpuLocal && schedule.pending_migration.is_some() {
                        return Err(Error::MigrationInProgress);
                    }
                    schedule
                        .wait
                        .arm(current, mobility, cpu)
                        .map_err(Error::from)
                })?
        })
    }

    pub fn finish_wait_shared(
        &self,
        cpu: CpuIndex,
        ticket: WaitTicket,
        completed: bool,
    ) -> Result<Option<WaitOutcome>, Error> {
        super::runtime::with_accounted_cpu(cpu, |local| {
            if local.current != ticket.thread() {
                return Err(Error::InvalidWaitRegistration);
            }
            local
                .thread_authority()
                .with_thread_mut(ticket.thread(), |_thread, schedule| {
                    if completed {
                        schedule
                            .wait
                            .finish_completed(ticket)
                            .map(Some)
                            .map_err(Error::from)
                    } else {
                        schedule.wait.finish_unqueued(ticket).map_err(Error::from)
                    }
                })?
        })
    }

    /// Prepares a park, or consumes an outcome that won before queue insertion.
    ///
    /// Queue membership and wait phase change under the current CPU lock.
    /// A `Park` result contains a prepared context switch; the caller still
    /// performs the handoff after leaving the scheduler's locked state.
    pub fn park_shared(
        &self,
        cpu: CpuIndex,
        wait_queue: &WaitQueue,
        ticket: WaitTicket,
    ) -> Result<PreparedWait, Error> {
        super::runtime::with_accounted_cpu(cpu, |local| {
            let current = local.current;
            if current != ticket.thread() {
                return Err(Error::InvalidWaitRegistration);
            }
            let pending =
                local
                    .thread_authority()
                    .with_thread(current, |_thread, schedule| {
                        if schedule.state != ThreadState::Running {
                            return Err(Error::CannotBlockIdle);
                        }
                        schedule
                            .wait
                            .pending_resolution(ticket)
                            .map_err(Error::from)
                    })??;
            match pending {
                PendingResolution::AlreadyCompleted => {
                    let outcome = local.thread_authority().with_thread_mut(
                        current,
                        |_thread, schedule| {
                            schedule.wait.finish_completed(ticket).map_err(Error::from)
                        },
                    )??;
                    return Ok(PreparedWait::Completed(outcome));
                }
                PendingResolution::Armed => {}
                _ => return Err(Error::InvalidWaitRegistration),
            }
            if local.handoff.current().is_some() {
                return Err(Error::ThreadTransitionInProgress);
            }
            // Preflight before changing queue topology or consuming a successor.
            let next = local
                .local_peek_ready()?
                .map(|ready| ready.id)
                .or(local.idle);
            let Some(next) = next.filter(|next| *next != current) else {
                local
                    .thread_authority()
                    .with_thread_mut(current, |_thread, schedule| {
                        schedule.wait.finish_unqueued(ticket).map_err(Error::from)
                    })??;
                return Err(Error::CurrentThreadMissing);
            };
            self.with_wait_queue(wait_queue, |queue| {
                // SAFETY: the reader pins the registry, this closure owns the
                // queue, and the candidate is protected by its CPU lock.
                let control = unsafe { self.registry.wait_control_authority() };
                let mut threads = queue::ControlQueueAuthority::new(control);
                let membership = QueueMembership::Waiting {
                    queue: wait_queue.identity(),
                };
                if let Err(error) = queue::control_push(&mut threads, queue, current, membership) {
                    scheduler_invariant(error);
                }
                let result =
                    local
                        .thread_authority()
                        .with_thread_mut(current, |_thread, schedule| {
                            schedule.wait.queue(ticket, wait_queue.identity())?;
                            schedule.state = ThreadState::Blocked;
                            Ok::<_, WaitRecordError>(())
                        });
                if !matches!(result, Ok(Ok(()))) {
                    scheduler_invariant(Error::InvalidWaitRegistration);
                }
            });
            match local.local_dequeue_ready() {
                Ok(Some(id)) if id == next => {}
                Ok(None) if local.idle == Some(next) => {}
                Ok(_) => scheduler_invariant(Error::QueueCorrupted),
                Err(error) => scheduler_invariant(error),
            }
            let switch = local
                .prepare_local_switch(
                    current,
                    next,
                    super::SwitchReason::Block,
                    super::SchedulePoint::Explicit,
                )
                .unwrap_or_else(|error| scheduler_invariant(error));
            Ok(PreparedWait::Park { switch, ticket })
        })
    }

    /// Arbitrates one generation-qualified wake, timeout or cancellation.
    ///
    /// Stale and already-completed tickets lose without running `on_commit`.
    /// The winner calls it under the owner CPU lock, after selecting the outcome
    /// and before publishing a queued thread as ready. It must not block or
    /// re-enter the scheduler. Resolving an armed wait need not enqueue a thread.
    pub fn resolve_wait_shared(
        &self,
        ticket: WaitTicket,
        outcome: WaitOutcome,
        preemption: WakePreemption,
        on_commit: impl FnOnce(),
    ) -> Result<ResolvedWait, Error> {
        let Some(cpu) = self.wait_owner(ticket.thread())? else {
            return Ok(lost());
        };
        super::runtime::with_accounted_cpu(cpu, |local| {
            self.resolve_on_cpu(local, ticket, outcome, preemption, on_commit)
        })
    }

    fn wait_owner(&self, id: ThreadId) -> Result<Option<CpuIndex>, Error> {
        match self.registry.with_thread(id, Thread::schedule_owner_cpu) {
            Ok(owner) => Ok(owner),
            Err(Error::ThreadNotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn resolve_on_cpu(
        &self,
        local: &mut CpuScheduler,
        ticket: WaitTicket,
        outcome: WaitOutcome,
        preemption: WakePreemption,
        on_commit: impl FnOnce(),
    ) -> Result<ResolvedWait, Error> {
        let pending =
            local
                .thread_authority()
                .with_thread(ticket.thread(), |_thread, schedule| {
                    schedule
                        .wait
                        .pending_resolution(ticket)
                        .map_err(Error::from)
                })??;
        match pending {
            PendingResolution::Stale | PendingResolution::AlreadyCompleted => Ok(lost()),
            PendingResolution::Armed => {
                complete_record(local, ticket, outcome);
                on_commit();
                Ok(ResolvedWait {
                    won: true,
                    ready: None,
                })
            }
            PendingResolution::Queued { queue } => {
                // SAFETY: the owner CPU lock prevents completion of this
                // queued registration, whose continuation retains the queue.
                let wait_queue =
                    unsafe { &*core::ptr::with_exposed_provenance::<WaitQueue>(queue) };
                self.with_wait_queue(wait_queue, |queue| {
                    self.unlink_waiter(queue, wait_queue, ticket.thread());
                    complete_record(local, ticket, outcome);
                });
                on_commit();
                Ok(ResolvedWait {
                    won: true,
                    ready: Some(ready_waiter(local, ticket, preemption)),
                })
            }
        }
    }

    fn unlink_waiter(&self, queue: &mut ThreadQueue, wait_queue: &WaitQueue, id: ThreadId) {
        // SAFETY: caller holds this queue's lock and the candidate's CPU
        // lock; the registry reader lane prevents slot removal or migration.
        let control = unsafe { self.registry.wait_control_authority() };
        let mut threads = queue::ControlQueueAuthority::new(control);
        if let Err(error) = queue::control_remove(
            &mut threads,
            queue,
            id,
            QueueMembership::Waiting {
                queue: wait_queue.identity(),
            },
        ) {
            scheduler_invariant(error);
        }
    }

    /// Removes one current queue head and makes it ready after `before_ready`.
    ///
    /// Head discovery is retried under the owner CPU lock so competing timeout
    /// or cancellation cannot select the same waiter twice. The callback runs
    /// at most once, outside the queue lock but inside the CPU lock; it must not
    /// block or re-enter the scheduler.
    pub fn notify_one_shared(
        &self,
        wait_queue: &WaitQueue,
        before_ready: impl FnOnce(ThreadId),
    ) -> Result<Option<(ThreadId, ReadyOutcome)>, Error> {
        let mut before_ready = Some(before_ready);
        loop {
            let head = self.wait_queue_snapshot(wait_queue)?.head;
            let Some(id) = head else {
                return Ok(None);
            };
            let Some(cpu) = self.wait_owner(id)? else {
                // A competing resolver may have removed and retired the head
                // before this reader began only if it was already stale. The
                // queue is rechecked before treating absence as corruption.
                if self.with_wait_queue(wait_queue, |queue| queue.head == Some(id)) {
                    return Err(Error::QueueCorrupted);
                }
                continue;
            };
            let result = super::runtime::with_accounted_cpu(cpu, |local| {
                let won = self.with_wait_queue(wait_queue, |queue| {
                    if queue.head != Some(id) {
                        return Ok(None);
                    }
                    let ticket = local
                        .thread_authority()
                        .with_thread(id, |_thread, schedule| {
                            schedule.wait.queued_ticket(id, wait_queue.identity())
                        })?
                        .ok_or(Error::QueueCorrupted)?;
                    self.unlink_waiter(queue, wait_queue, id);
                    complete_record(local, ticket, WaitOutcome::Notified);
                    Ok::<_, Error>(Some(ticket))
                })?;
                let Some(ticket) = won else {
                    return Ok(None);
                };
                if let Some(callback) = before_ready.take() {
                    callback(id);
                }
                let ready = ready_waiter(local, ticket, WakePreemption::Policy);
                Ok::<_, Error>(Some((id, ready)))
            })?;
            if result.is_some() {
                return Ok(result);
            }
        }
    }

    pub fn cancel_waiter_shared(
        &self,
        wait_queue: &WaitQueue,
        id: ThreadId,
    ) -> Result<ResolvedWait, Error> {
        let Some(cpu) = self.wait_owner(id)? else {
            return Ok(lost());
        };
        super::runtime::with_accounted_cpu(cpu, |local| {
            let ticket = local
                .thread_authority()
                .with_thread(id, |_thread, schedule| {
                    schedule.wait.queued_ticket(id, wait_queue.identity())
                })?;
            match ticket {
                Some(ticket) => self.resolve_on_cpu(
                    local,
                    ticket,
                    WaitOutcome::Cancelled,
                    WakePreemption::Policy,
                    || {},
                ),
                None => Ok(lost()),
            }
        })
    }
}

fn lost() -> ResolvedWait {
    ResolvedWait {
        won: false,
        ready: None,
    }
}

fn complete_record(local: &mut CpuScheduler, ticket: WaitTicket, outcome: WaitOutcome) {
    let result = local
        .thread_authority()
        .with_thread_mut(ticket.thread(), |_thread, schedule| {
            schedule.wait.complete(ticket, outcome)
        });
    if !matches!(result, Ok(Ok(()))) {
        scheduler_invariant(Error::InvalidWaitRegistration);
    }
}

fn ready_waiter(
    local: &mut CpuScheduler,
    ticket: WaitTicket,
    preemption: WakePreemption,
) -> ReadyOutcome {
    let id = ticket.thread();
    if local
        .thread_authority()
        .with_thread(id, |_thread, schedule| {
            schedule.state == ThreadState::Blocked
        })
        != Ok(true)
    {
        scheduler_invariant(Error::InvalidThreadState);
    }
    local
        .local_enqueue_ready(id, false)
        .unwrap_or_else(|error| scheduler_invariant(error));
    crate::tracepoint!(
        Scheduler,
        "sched_wakeup",
        thread = id.get(),
        target_cpu = local.index.get() as u64,
        wait = ticket.generation(),
        waker = crate::diagnostics::trace::actor()
    );
    let candidate = local
        .thread_authority()
        .with_thread(id, |_thread, schedule| schedule.scheduling)
        .unwrap_or_else(|error| scheduler_invariant(error));
    let current = local.current;
    local
        .thread_authority()
        .with_thread_mut(current, |_thread, schedule| {
            if preemption == WakePreemption::FairBoundary
                && candidate == SchedulingPolicy::Fair
                && schedule.state == ThreadState::Running
                && schedule.scheduling == SchedulingPolicy::Fair
            {
                schedule.expire_fair_slice();
            }
        })
        .unwrap_or_else(|error| scheduler_invariant(error));
    ReadyOutcome {
        changed: true,
        target_cpu: local.index,
    }
}
