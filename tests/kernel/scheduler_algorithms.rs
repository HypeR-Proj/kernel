// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! End-to-end Budget preemption and event-driven comparator programming.
//!
//! Workers deliberately perform no yield, sleep, allocation, or scheduler call
//! in their workload. Progress therefore requires actual interrupt preemption.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::task::thread::ThreadId;
use {crate::task, crate::time};
use {
    crate::task::policy::BudgetParameters, crate::task::policy::SchedulingParameters,
    crate::task::policy::SchedulingRole, crate::task::policy::ThreadPriority,
};
use {crate::task::scheduler, crate::task::scheduler::CpuMask};
use {
    crate::task::scheduling_authority::SCHEDULING_RIGHTS,
    crate::task::scheduling_authority::SchedulingAuthority,
    crate::task::scheduling_authority::SchedulingLimits,
};

const PERIOD_NS: u64 = hyper::config::SCHED_PERIOD_MS as u64 * 1_000_000;
const NO_WORKER: usize = usize::MAX;
static RELEASE: AtomicBool = AtomicBool::new(false);
static FAILURE: AtomicBool = AtomicBool::new(false);
static TARGET_CPU: AtomicUsize = AtomicUsize::new(0);
static PROGRESS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static LAST_WORKER: AtomicUsize = AtomicUsize::new(NO_WORKER);
static SWITCHES: AtomicUsize = AtomicUsize::new(0);

pub(super) enum Error {
    Scheduler(scheduler::Error),
    Sleep(task::SleepError),
    Quiescence(super::support::QuiescenceError),
    Contract(&'static str),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scheduler(error) => f.debug_tuple("Scheduler").field(error).finish(),
            Self::Sleep(error) => f.debug_tuple("Sleep").field(error).finish(),
            Self::Quiescence(error) => f.debug_tuple("Quiescence").field(error).finish(),
            Self::Contract(error) => f.debug_tuple("Contract").field(error).finish(),
        }
    }
}

impl From<scheduler::Error> for Error {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}
impl From<task::SleepError> for Error {
    fn from(error: task::SleepError) -> Self {
        Self::Sleep(error)
    }
}
impl From<super::support::QuiescenceError> for Error {
    fn from(error: super::support::QuiescenceError) -> Self {
        Self::Quiescence(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    super::support::quiesce_workers()?;
    let online = crate::cpu::online_cpu_count();
    let isolated = online > 1;
    let target = CpuIndex::new(online - 1).ok_or(Error::Contract("invalid target CPU"))?;
    TARGET_CPU.store(target.get(), Ordering::Release);
    RELEASE.store(false, Ordering::Release);
    FAILURE.store(false, Ordering::Release);
    LAST_WORKER.store(NO_WORKER, Ordering::Release);
    SWITCHES.store(0, Ordering::Release);
    for progress in &PROGRESS {
        progress.store(0, Ordering::Release);
    }

    // Q=P and B=P specifically exercise period rollover: replenishing Q must
    // not replenish the uninterrupted burst and let the incumbent run forever.
    let budget = BudgetParameters::new(PERIOD_NS, PERIOD_NS, PERIOD_NS)
        .ok_or(Error::Contract("invalid test budget"))?;
    let parameters =
        SchedulingParameters::budget(budget, ThreadPriority::NORMAL, SchedulingRole::Ordinary);
    let mut workers: [Option<(ThreadId, bool)>; 2] = [None; 2];
    let result = (|| {
        for (index, slot) in workers.iter_mut().enumerate() {
            let id = scheduler::kthread_create_per_cpu("budget-test", target, worker, index)?;
            *slot = Some((id, false));
            scheduler::configure_scheduling(
                id,
                parameters,
                SchedulingAuthority::new(SCHEDULING_RIGHTS),
                SchedulingLimits::SYSTEM,
            )?;
            if scheduler::set_thread_affinity(id, CpuMask::ALL)
                != Err(scheduler::Error::MigrationUnsupported)
                || !matches!(
                    scheduler::migrate_thread(id, CpuIndex::BOOT),
                    Err(scheduler::Error::MigrationUnsupported | scheduler::Error::CpuNotAllowed)
                )
                || scheduler::thread_placement(id)? != (target, CpuMask::single(target))
            {
                return Err(Error::Contract(
                    "CPU-owned Budget worker accepted migration",
                ));
            }
        }

        ready(&mut workers[0])?;
        wait("first Budget worker did not run", || {
            PROGRESS[0].load(Ordering::Acquire) != 0
        })?;
        if isolated {
            wait("sole Budget worker retained a scheduler deadline", || {
                time::scheduler_deadline_for_test(target).deadline.is_none()
            })?;
            let before = time::scheduler_deadline_for_test(target);
            let progress = PROGRESS[0].load(Ordering::Acquire);
            task::sleep_ns(PERIOD_NS * 3)?;
            let after = time::scheduler_deadline_for_test(target);
            if after.deadline.is_some()
                || after.expiries != before.expiries
                || PROGRESS[0].load(Ordering::Acquire) == progress
            {
                return Err(Error::Contract(
                    "sole Budget worker retained a scheduler tick",
                ));
            }
        }
        // On one CPU the observer itself is a valid runnable competitor. Only
        // the quiet-CPU assertion above is skipped; Q=P preemption still runs.
        let solo = time::scheduler_deadline_for_test(target);
        ready(&mut workers[1])?;
        wait(
            "remote enqueue did not arm and expire the scheduler comparator",
            || {
                let observed = time::scheduler_deadline_for_test(target);
                observed.generation > solo.generation && observed.expiries > solo.expiries
            },
        )?;
        wait(
            "Q=P workers did not rotate across budget replenishment",
            || {
                PROGRESS
                    .iter()
                    .all(|count| count.load(Ordering::Acquire) != 0)
                    && SWITCHES.load(Ordering::Acquire) >= 5
            },
        )?;
        if FAILURE.load(Ordering::Acquire) {
            return Err(Error::Contract(
                "worker escaped its CPU or received an invalid index",
            ));
        }
        Ok(())
    })();

    // Every worker sees this without needing a scheduler entry. Even an
    // assertion failure therefore leaves no runnable spin worker behind.
    RELEASE.store(true, Ordering::Release);
    for (id, started) in workers.into_iter().flatten() {
        if !started {
            scheduler::discard_dormant_kernel_thread(id)?;
        }
    }
    super::support::quiesce_workers()?;
    result?;
    crate::pr_info!("HypeR test: Budget Q=P rotation, wake rearm and CPU ownership passed");
    if isolated {
        crate::pr_info!("HypeR test: isolated Budget worker has no scheduler timer expiries");
    }
    Ok(())
}

fn ready(slot: &mut Option<(ThreadId, bool)>) -> Result<(), Error> {
    let Some((id, started)) = slot else {
        return Err(Error::Contract("worker was not created"));
    };
    scheduler::thread_ready(*id)?;
    *started = true;
    Ok(())
}

fn wait(failure: &'static str, mut condition: impl FnMut() -> bool) -> Result<(), Error> {
    let timeout = task::TEST_PROGRESS_TIMEOUT_NS.max(PERIOD_NS * 12);
    if task::wait_for_test_progress(timeout, || Ok::<_, Error>(condition()))? {
        Ok(())
    } else {
        Err(Error::Contract(failure))
    }
}

extern "C" fn worker(index: usize) {
    let Some(progress) = PROGRESS.get(index) else {
        FAILURE.store(true, Ordering::Release);
        return;
    };
    if crate::cpu::current_index().map(CpuIndex::get) != Some(TARGET_CPU.load(Ordering::Acquire)) {
        FAILURE.store(true, Ordering::Release);
        return;
    }
    while !RELEASE.load(Ordering::Acquire) {
        if LAST_WORKER.swap(index, Ordering::AcqRel) != index {
            SWITCHES.fetch_add(1, Ordering::Release);
        }
        progress.fetch_add(1, Ordering::Release);
        for _ in 0..64 {
            core::hint::spin_loop();
        }
    }
}
