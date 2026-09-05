// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared lifecycle support for bare-metal kernel self-tests.

use crate::kernel::task::scheduler;

const MAX_QUIESCENCE_PASSES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct QuiescenceSnapshot {
    threads: usize,
    ready: usize,
    running: usize,
    blocked: usize,
    migrating: usize,
    retirements_in_progress: usize,
    idle: usize,
    idle_class_threads: usize,
    online_cpus: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QuiescenceError {
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    Timeout(QuiescenceSnapshot),
}

impl From<scheduler::Error> for QuiescenceError {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<crate::kernel::task::SleepError> for QuiescenceError {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

/// Retires every transient test Thread and returns the stable boot population.
///
/// Worker completion publishes the worker's last shared-state access; it does
/// not prove that the entry point returned, the exit trampoline ran, or the
/// scheduler reaper dropped the stack. Quiescence is reached only when the
/// bootstrap Thread, permanent kernel-service workers, and one idle Thread per
/// online CPU remain.
pub(super) fn quiesce_workers() -> Result<scheduler::Statistics, QuiescenceError> {
    let service_workers = crate::kernel::log::permanent_worker_count_for_test()
        + scheduler::permanent_worker_count_for_test();
    let mut last = QuiescenceSnapshot {
        threads: 0,
        ready: 0,
        running: 0,
        blocked: 0,
        migrating: 0,
        retirements_in_progress: 0,
        idle: 0,
        idle_class_threads: 0,
        online_cpus: crate::kernel::cpu::online_cpu_count(),
    };

    for _ in 0..MAX_QUIESCENCE_PASSES {
        scheduler::yield_now()?;
        let statistics = scheduler::statistics()?;
        let online_cpus = crate::kernel::cpu::online_cpu_count();
        last = QuiescenceSnapshot {
            threads: statistics.threads,
            ready: statistics.ready,
            running: statistics.running,
            blocked: statistics.blocked,
            migrating: statistics.migrating,
            retirements_in_progress: statistics.retirements_in_progress,
            idle: statistics.idle,
            idle_class_threads: statistics.idle_class_threads,
            online_cpus,
        };
        if statistics.retirements_in_progress != 0 {
            // A remote CPU may own the detached Thread's lock-external
            // destructor. A local yield with no ready peer does not provide
            // physical CPU progress, particularly under QEMU TCG. Sleeping
            // lets that legitimate in-flight owner release-publish completion.
            crate::kernel::task::sleep_ms(1)?;
            continue;
        }
        if statistics.ready == 0
            && statistics.blocked == service_workers
            && statistics.migrating == 0
            && statistics.running == 1
            && statistics.idle == online_cpus
            && statistics.idle_class_threads == online_cpus
            && statistics.threads
                == online_cpus
                    .saturating_add(1)
                    .saturating_add(service_workers)
        {
            return Ok(statistics);
        }

        // A non-quiescent owner may be executing on another physical CPU.
        // Repeated local yields only advance the boot CPU and can exhaust this
        // bounded poll before QEMU TCG schedules the remote owner. The timer
        // wait preserves local scheduler progress while yielding host time to
        // every online CPU.
        crate::kernel::task::sleep_ms(1)?;
    }
    Err(QuiescenceError::Timeout(last))
}
