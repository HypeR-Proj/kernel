// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Elapsed execution accounting and one-shot decisions under one CPU lock.

use super::super::algorithms::{
    PolicyRuntime,
    arbitration::{Candidate, DispatchMode},
};
use super::{CPU_SCHEDULERS, CpuScheduler, Error};
use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{AtomicU64, Ordering};
use {crate::task::policy::FairBackend, crate::task::policy::SchedulingPolicy};
use {crate::task::thread::ThreadId, crate::task::thread::ThreadState};

static BACK_TURN: AtomicU64 = AtomicU64::new(1 << 63);
static FRONT_TURN: AtomicU64 = AtomicU64::new(1 << 63);

pub(super) fn prepare_enqueue(
    schedule: &mut crate::task::thread::ThreadScheduleState,
    floor: u128,
    front: bool,
    now: u64,
) {
    schedule.runtime.epoch_refresh(now, schedule.parameters);
    if let PolicyRuntime::Fair(runtime) = &mut schedule.runtime {
        if let Some(origin) = schedule.fair_origin.take() {
            runtime.migrate_floor(origin, floor);
        }
        runtime.admit(floor);
    }
    if front && matches!(schedule.scheduling, SchedulingPolicy::Fifo { .. }) {
        schedule.ready_turn = FRONT_TURN.fetch_sub(1, Ordering::Relaxed) - 1;
        if schedule.ready_turn == 0 {
            hyper::debug::invariant_failure("FIFO turn namespace exhausted");
        }
    } else if !front || schedule.ready_turn == 0 {
        schedule.ready_turn = BACK_TURN.fetch_add(1, Ordering::Relaxed);
        if schedule.ready_turn == u64::MAX {
            hyper::debug::invariant_failure("scheduler turn namespace exhausted");
        }
    }
}

static DEADLINE_GENERATION: PerCpu<AtomicU64> =
    PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);

pub(in crate::task::scheduler) fn deadline_is_current(cpu: CpuIndex, generation: u64) -> bool {
    DEADLINE_GENERATION[cpu].load(Ordering::Acquire) == generation
}

pub(super) fn with_accounted_cpu<R>(
    cpu: CpuIndex,
    operation: impl FnOnce(&mut CpuScheduler) -> Result<R, Error>,
) -> Result<R, Error> {
    CPU_SCHEDULERS[cpu].with(|slot| {
        let local = slot.as_mut().ok_or(Error::CpuNotRegistered)?;
        local.account_current()?;
        let result = operation(local);
        local.refresh_deadline()?;
        result
    })
}

pub(in crate::task::scheduler) fn account_all_cpus() -> Result<(), Error> {
    for index in 0..hyper::cpu::MAX_CPUS {
        let Some(cpu) = CpuIndex::new(index) else {
            break;
        };
        CPU_SCHEDULERS[cpu].with(|slot| {
            if let Some(local) = slot.as_mut() {
                local.account_current()?;
            }
            Ok::<_, Error>(())
        })?;
    }
    Ok(())
}

impl CpuScheduler {
    /// Algorithm and arbitration comparisons are independent of context type.
    #[inline(never)] // Keep the measured IRQ-tail stack boundary independent of callers.
    pub(super) fn choose_ready(
        &mut self,
        yielding: bool,
    ) -> Result<Option<(ThreadId, bool)>, Error> {
        let current = self.current_candidate()?;
        let selection = self.local_selection(current)?;
        let id = self.current;
        let (expired, parameters, deferred) =
            self.thread_authority()
                .with_thread(id, |_thread, schedule| {
                    (
                        match schedule.runtime {
                            PolicyRuntime::Fair(r) => r.remaining_ns() == 0,
                            PolicyRuntime::Budget(r) => r.burst_remaining_ns() == 0,
                            _ => false,
                        },
                        schedule.parameters,
                        schedule.deferred_fifo_placement,
                    )
                })?;
        let key = current.and_then(|candidate| {
            self.window
                .candidate_key(self.now(), candidate, selection.demand)
        });
        let should_switch = match (selection.ready, key) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(ready), Some((key, _))) => {
                let higher_band = (ready.key.urgency, ready.key.rank, ready.key.algorithm_rank)
                    < (key.urgency, key.rank, key.algorithm_rank);
                let equal_band = (ready.key.urgency, ready.key.rank, ready.key.algorithm_rank)
                    == (key.urgency, key.rank, key.algorithm_rank);
                higher_band
                    || match parameters.policy() {
                        SchedulingPolicy::Fifo { .. } => equal_band
                            && (ready.key.algorithm < key.algorithm
                                || (ready.key.algorithm == key.algorithm
                                    && (yielding
                                        || deferred
                                            == Some(
                                                crate::task::thread::DeferredFifoPlacement::Tail,
                                            )))),
                        SchedulingPolicy::Fair => {
                            yielding
                                || (expired
                                    && equal_band
                                    && (parameters.fair_backend() == FairBackend::RoundRobin
                                        || ready.key < key))
                        }
                        SchedulingPolicy::Budget => yielding || (expired && equal_band),
                        SchedulingPolicy::Idle => true,
                    }
            }
        };
        if expired || yielding {
            self.thread_authority()
                .with_thread_mut(id, |_thread, schedule| {
                    schedule.runtime.finish_turn(parameters)
                })?;
        }
        self.thread_authority()
            .with_thread_mut(id, |_thread, schedule| {
                schedule.deferred_fifo_placement = None
            })?;
        Ok(if should_switch {
            selection
                .ready
                .map(|ready| (ready.id, !expired && !yielding))
        } else {
            None
        })
    }
    pub(super) fn now(&self) -> u64 {
        self.last_account_ns.unwrap_or(0)
    }

    pub(super) fn account_current(&mut self) -> Result<(), Error> {
        let Ok(now) = crate::time::monotonic_nanoseconds() else {
            return Ok(());
        };
        let start = self.last_account_ns.replace(now).unwrap_or(now);
        let elapsed = now.checked_sub(start).ok_or(Error::PreemptionInvariant)?;
        let cpu = self.index;
        let current = self.current;
        let (parameters, fair_pass) =
            self.thread_authority()
                .with_thread_mut(current, |thread, schedule| {
                    thread.account_runtime_ns(elapsed);
                    super::super::account_cpu_time(cpu, thread.role(), elapsed);
                    if !schedule.runtime.account(schedule.parameters, start, now) {
                        return Err(Error::InvalidSchedulingParameters);
                    }
                    Ok((
                        schedule.parameters,
                        match schedule.runtime {
                            PolicyRuntime::Fair(runtime) => Some(runtime.pass()),
                            _ => None,
                        },
                    ))
                })??;
        if self.run_queue.fair_len() == self.run_queue.budget_len()
            && let Some(pass) = fair_pass
        {
            self.fair_floor = self.fair_floor.max(pass);
        }
        if !self
            .window
            .account_dispatch(start, now, parameters, self.dispatch_mode)
        {
            return Err(Error::PreemptionInvariant);
        }
        Ok(())
    }

    pub(super) fn current_candidate(&mut self) -> Result<Option<Candidate>, Error> {
        let now = self.now();
        let current = self.current;
        self.thread_authority()
            .with_thread_mut(current, |_thread, schedule| {
                if schedule.state != ThreadState::Running {
                    return None;
                }
                schedule.runtime.epoch_refresh(now, schedule.parameters);
                Some(Candidate {
                    parameters: schedule.parameters,
                    fair_pass: match schedule.runtime {
                        PolicyRuntime::Fair(r) => r.pass(),
                        _ => 0,
                    },
                    turn: schedule.ready_turn,
                    budget_available: match schedule.runtime {
                        PolicyRuntime::Budget(r) => r.remaining_ns() != 0,
                        _ => true,
                    },
                })
            })
    }

    pub(super) fn refresh_deadline(&mut self) -> Result<(), Error> {
        // Scheduler ownership exists before clocksource/timer initialization.
        // The first accounted runtime event after clock discovery establishes
        // the epoch; there is no comparator deadline to publish before that.
        if self.last_account_ns.is_none() {
            return Ok(());
        }
        let now = self.now();
        let current = self.current_candidate()?;
        let selection = self.local_selection(current)?;
        let competition = self.run_queue.len() != 0;
        let mode = current
            .and_then(|candidate| self.window.candidate_key(now, candidate, selection.demand));
        let mut deadline = None;
        if let Some((_, mode)) = mode {
            self.dispatch_mode = mode;
            let id = self.current;
            let budget_peer = self.run_queue.budget_len() != 0;
            deadline = self
                .thread_authority()
                .with_thread_mut(id, |_thread, schedule| {
                    if let (PolicyRuntime::Budget(runtime), Some(budget)) =
                        (schedule.runtime, schedule.parameters.budget_parameters())
                        && competition
                        && !budget_peer
                    {
                        // A lower-ranked Native contender needs Q/floor, not a
                        // gratuitous burst interrupt that cannot change current.
                        return now.checked_add(runtime.remaining_ns()).map(|expiry| {
                            expiry.min(
                                (now - now % budget.period_ns()).saturating_add(budget.period_ns()),
                            )
                        });
                    }
                    // Exhausted Budget peers cannot use a Fair quantum. The
                    // CPU window still arms their replenishment deadline below.
                    let turn_competition = competition
                        && (schedule.parameters.policy() != SchedulingPolicy::Fair
                            || selection.ready.is_some());
                    schedule.runtime.next_deadline(
                        now,
                        schedule.parameters,
                        turn_competition,
                        mode == DispatchMode::Borrow,
                    )
                })?;
            deadline = earliest(
                deadline,
                self.window
                    .next_deadline(now, mode, competition, selection.demand),
            );
        } else if competition {
            // The running entity exhausted its entitlement while a peer can run.
            deadline = Some(now);
        }
        if deadline == self.armed_deadline_ns {
            return Ok(());
        }
        self.armed_deadline_ns = deadline;
        self.deadline_generation = self
            .deadline_generation
            .checked_add(1)
            .ok_or(Error::PreemptionInvariant)?;
        DEADLINE_GENERATION[self.index].store(self.deadline_generation, Ordering::Release);
        let hardware_deadline = deadline
            .map(crate::time::deadline_from_monotonic_nanoseconds)
            .transpose()
            .map_err(|_| Error::PreemptionInvariant)?;
        crate::time::publish_scheduler_deadline(
            self.index,
            self.deadline_generation,
            hardware_deadline,
        )
        .map_err(|_| Error::PreemptionInvariant)
    }
}

fn earliest(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(in crate::task::scheduler) fn expire_current_turn(cpu: CpuIndex) -> Result<(), Error> {
    with_accounted_cpu(cpu, |local| {
        let id = local.current;
        local
            .thread_authority()
            .with_thread_mut(id, |_thread, schedule| schedule.expire_fair_slice())
    })
}
