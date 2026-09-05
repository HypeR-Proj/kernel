// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared lifecycle support for bare-metal kernel self-tests.

use crate::kernel::task::scheduler;

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
    let mut stable_statistics = None;

    let quiescent = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || {
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
            let stable = statistics.retirements_in_progress == 0
                && statistics.ready == 0
                && statistics.blocked == service_workers
                && statistics.migrating == 0
                && statistics.running == 1
                && statistics.idle == online_cpus
                && statistics.idle_class_threads == online_cpus
                && statistics.threads
                    == online_cpus
                        .saturating_add(1)
                        .saturating_add(service_workers);
            if stable {
                stable_statistics = Some(statistics);
            }
            Ok::<_, QuiescenceError>(stable)
        },
    )?;
    if quiescent {
        stable_statistics.ok_or(QuiescenceError::Timeout(last))
    } else {
        Err(QuiescenceError::Timeout(last))
    }
}
