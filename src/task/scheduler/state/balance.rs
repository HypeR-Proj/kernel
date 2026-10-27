// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded Fair placement and Ready-only automatic migration.

use hyper::cpu::CpuIndex;

use super::super::balance_policy::{improves_balance, least_loaded};
use super::super::{Error, MigrationStatus};
use super::{CPU_SCHEDULERS, ReadyOutcome, Scheduler, scheduler_invariant};
use {
    crate::task::policy::CpuMask, crate::task::policy::PlacementPolicy,
    crate::task::policy::SchedulingPolicy,
};
use {
    crate::task::thread::ExecutionKind, crate::task::thread::Thread, crate::task::thread::ThreadId,
    crate::task::thread::ThreadState,
};

const SCAN_BUDGET: usize = 64;
const MOVE_BUDGET: usize = 4;

/// One event visits the registry extent captured at the start exactly once.
/// Additional requests do not reset an unfinished pass to its first slot.
pub(super) struct BalanceCursor {
    next: usize,
    end: usize,
}

impl BalanceCursor {
    pub(super) const fn new() -> Self {
        Self { next: 0, end: 0 }
    }

    fn begin_if_idle(&mut self, extent: usize) {
        if !self.has_more() {
            self.next = 0;
            self.end = extent;
        }
    }

    fn take(&mut self) -> Option<usize> {
        if !self.has_more() {
            return None;
        }
        let next = self.next;
        self.next += 1;
        Some(next)
    }

    const fn has_more(&self) -> bool {
        self.next < self.end
    }
}

pub(in crate::task::scheduler) struct BalancePass {
    pub(in crate::task::scheduler) outcomes: [Option<ReadyOutcome>; MOVE_BUDGET],
    pub(in crate::task::scheduler) more: bool,
}

#[derive(Clone, Copy, Default)]
struct CpuLoad {
    runnable: u32,
    realtime: u32,
    budget: u32,
}

/// Fixed-size placement scratch allocated before scheduler publication.
/// It is leased only under the coordinator; no reference survives a pass.
pub(super) struct BalanceWorkspace {
    loads: [CpuLoad; hyper::cpu::MAX_CPUS],
    outcomes: [Option<ReadyOutcome>; MOVE_BUDGET],
}

impl BalanceWorkspace {
    pub(super) fn new() -> Self {
        Self {
            loads: [CpuLoad::default(); hyper::cpu::MAX_CPUS],
            outcomes: [None; MOVE_BUDGET],
        }
    }
}

impl Scheduler {
    /// Taking the owner prevents nested placement from reusing live scratch.
    /// The coordinator excludes other CPUs, and this callback cannot sleep or
    /// switch Threads. Restore the owner on both success and recoverable error;
    /// kernel panics abort instead of unwinding through an unfinished lease.
    fn with_balance_workspace<R>(
        &mut self,
        operation: impl FnOnce(&mut Self, &mut BalanceWorkspace) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut workspace = self
            .balance_workspace
            .take()
            .ok_or(Error::InvalidThreadState)?;
        let result = operation(self, &mut workspace);
        self.balance_workspace = Some(workspace);
        result
    }

    /// Initial placement is separate from affinity validation: the latter may
    /// already own a source CPU lock and must never acquire remote CPU locks.
    pub(super) fn select_initial_cpu(
        &mut self,
        preferred: CpuIndex,
        affinity: CpuMask,
    ) -> Result<CpuIndex, Error> {
        if affinity.is_empty() {
            return Err(Error::EmptyCpuAffinity);
        }
        self.with_balance_workspace(|scheduler, workspace| {
            scheduler.balance_loads(&mut workspace.loads)?;
            scheduler.select_cpu_from_loads(preferred, affinity, &workspace.loads)
        })
    }

    fn select_cpu_from_loads(
        &self,
        preferred: CpuIndex,
        affinity: CpuMask,
        loads: &[CpuLoad; hyper::cpu::MAX_CPUS],
    ) -> Result<CpuIndex, Error> {
        let candidates = || {
            (0..hyper::cpu::MAX_CPUS)
                .filter_map(CpuIndex::new)
                .filter(|cpu| affinity.contains(*cpu) && self.cpu_is_schedulable(*cpu))
        };
        let avoid_realtime = candidates().any(|cpu| loads[cpu.get()].realtime == 0);
        least_loaded(
            preferred.get(),
            candidates()
                .filter(|cpu| !avoid_realtime || loads[cpu.get()].realtime == 0)
                .map(|cpu| {
                    (
                        cpu.get(),
                        loads[cpu.get()]
                            .runnable
                            .saturating_mul(4)
                            .saturating_add(loads[cpu.get()].budget.saturating_mul(2)),
                    )
                }),
        )
        .and_then(CpuIndex::new)
        .ok_or(Error::NoRegisteredCpuInAffinity)
    }

    /// Takes only one CPU lock at a time. Exclusive coordination excludes
    /// enqueue, wake, park and retirement; local switches conserve runnable
    /// counts, so the resulting loads remain valid throughout this pass.
    fn balance_loads(&self, loads: &mut [CpuLoad; hyper::cpu::MAX_CPUS]) -> Result<(), Error> {
        if self.active_domain.is_some() {
            return Err(Error::InvalidThreadState);
        }
        loads.fill(CpuLoad::default());
        for cpu in (0..hyper::cpu::MAX_CPUS).filter_map(CpuIndex::new) {
            if !self.cpu_is_schedulable(cpu) {
                continue;
            }
            loads[cpu.get()] = CPU_SCHEDULERS[cpu].with(|slot| {
                let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
                let current = local.current;
                let (running, realtime, budget) =
                    local
                        .thread_authority()
                        .with_thread(current, |_thread, schedule| match schedule.state {
                            ThreadState::Running => Ok((
                                true,
                                matches!(schedule.scheduling, SchedulingPolicy::Fifo { .. }),
                                schedule.scheduling == SchedulingPolicy::Budget,
                            )),
                            ThreadState::Idle => Ok((false, false, false)),
                            _ => Err(Error::InvalidThreadState),
                        })??;
                Ok::<_, Error>(CpuLoad {
                    runnable: u32::try_from(local.run_queue.len())
                        .map_err(|_| Error::QueueCorrupted)?
                        + u32::from(running),
                    budget: local.run_queue.budget_len() as u32 + u32::from(running && budget),
                    realtime: u32::try_from(local.run_queue.real_time_len())
                        .map_err(|_| Error::QueueCorrupted)?
                        + u32::from(running && realtime),
                })
            })?;
        }
        Ok(())
    }

    pub(in crate::task::scheduler) fn balance_ready(
        &mut self,
        excluded: ThreadId,
    ) -> Result<BalancePass, Error> {
        self.with_balance_workspace(|scheduler, workspace| {
            scheduler.balance_ready_in(excluded, workspace)
        })
    }

    fn balance_ready_in(
        &mut self,
        excluded: ThreadId,
        workspace: &mut BalanceWorkspace,
    ) -> Result<BalancePass, Error> {
        let BalanceWorkspace { loads, outcomes } = workspace;
        outcomes.fill(None);
        // Balancing executes in an arbitrary caller, not a dedicated worker.
        // Its current vCPU/Native execution is real load on the source CPU.
        self.balance_loads(loads)?;
        let mut moved = 0;
        self.balance_cursor
            .begin_if_idle(self.registry.balance_slot_count());
        for _ in 0..SCAN_BUDGET {
            let Some(slot) = self.balance_cursor.take() else {
                break;
            };
            let Some(id) = self.registry.balance_candidate(slot) else {
                continue;
            };
            let Some(source) = self.registry.with_thread(id, Thread::schedule_owner_cpu)? else {
                continue;
            };
            let outcome = self.with_cpu_schedule_stored(source, |scheduler| {
                let thread = scheduler.thread(id)?;
                if id == excluded
                    || thread.state() != ThreadState::Ready
                    || !matches!(
                        thread.scheduling_policy(),
                        SchedulingPolicy::Fair | SchedulingPolicy::Budget
                    )
                    || scheduler
                        .thread_mut(id)?
                        .apply_schedule(|schedule| schedule.auto_migrated)
                    || thread.placement_policy() != PlacementPolicy::Movable
                    || thread.pending_migration().is_some()
                    || !matches!(
                        thread.execution_kind(),
                        ExecutionKind::Kernel | ExecutionKind::User | ExecutionKind::Vcpu
                    )
                    || scheduler
                        .switching_from(source)?
                        .is_some_and(|handoff| handoff.thread == id)
                {
                    return Ok(None);
                }
                let target = least_loaded(
                    source.get(),
                    (0..hyper::cpu::MAX_CPUS)
                        .filter_map(CpuIndex::new)
                        .filter(|cpu| {
                            *cpu != source
                                && scheduler.cpu_is_schedulable(*cpu)
                                && thread.can_run_on(*cpu)
                                && thread.wait_record().permits_assignment(*cpu)
                                && loads[cpu.get()].realtime == 0
                                && improves_balance(
                                    loads[source.get()].runnable,
                                    loads[cpu.get()].runnable,
                                )
                        })
                        .map(|cpu| {
                            (
                                cpu.get(),
                                loads[cpu.get()]
                                    .runnable
                                    .saturating_mul(4)
                                    .saturating_add(loads[cpu.get()].budget.saturating_mul(2)),
                            )
                        }),
                )
                .and_then(CpuIndex::new);
                let Some(target) = target else {
                    return Ok(None);
                };
                // Keep this source lock from the Ready/context-save checks
                // through removal. Existing migration defers target enqueue
                // until this lock is released; two CPU locks never nest.
                scheduler
                    .thread_mut(id)?
                    .apply_schedule(|schedule| schedule.auto_migrated = true);
                let migration = scheduler.migrate_thread(id, target)?;
                if migration.status != MigrationStatus::Completed
                    || migration.source_reschedule.is_some()
                {
                    scheduler_invariant(Error::InvalidThreadState);
                }
                migration
                    .target_ready
                    .ok_or(Error::InvalidThreadState)
                    .map(|ready| {
                        Some((
                            ready,
                            thread.scheduling_policy() == SchedulingPolicy::Budget,
                        ))
                    })
            })?;
            if let Some((outcome, budget)) = outcome {
                loads[source.get()].runnable -= 1;
                loads[outcome.target_cpu.get()].runnable += 1;
                if budget {
                    loads[source.get()].budget -= 1;
                    loads[outcome.target_cpu.get()].budget += 1;
                }
                outcomes[moved] = Some(outcome);
                moved += 1;
                if moved == MOVE_BUDGET {
                    break;
                }
            }
        }
        Ok(BalancePass {
            outcomes: *outcomes,
            more: self.balance_cursor.has_more(),
        })
    }
}
