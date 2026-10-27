// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-owned CPU-time accounting and immutable publication snapshots.
//!
//! Each closed execution interval updates one CPU-local category. Every
//! category is an independent monotonic atomic value, so a reader cannot see a
//! partially published multi-field transaction: no such write transaction
//! exists. A system snapshot is intentionally weakly consistent across CPUs.

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{AtomicU64, Ordering};

use crate::task::ThreadRole;

struct CpuTime {
    idle: AtomicU64,
    kernel_thread: AtomicU64,
    user_thread: AtomicU64,
    vcpu: AtomicU64,
}

impl CpuTime {
    const fn new() -> Self {
        Self {
            idle: AtomicU64::new(0),
            kernel_thread: AtomicU64::new(0),
            user_thread: AtomicU64::new(0),
            vcpu: AtomicU64::new(0),
        }
    }

    fn account(&self, role: ThreadRole, elapsed: u64) {
        let counter = match role {
            ThreadRole::Idle => &self.idle,
            ThreadRole::User => &self.user_thread,
            ThreadRole::Vcpu => &self.vcpu,
            ThreadRole::Bootstrap | ThreadRole::Kernel => &self.kernel_thread,
        };
        // These counters are observations, not synchronization. Relaxed
        // atomicity prevents races with readers; scheduler ownership orders
        // the writer itself.
        counter.fetch_add(elapsed, Ordering::Relaxed);
    }
}

static CPU_TIME: PerCpu<CpuTime> = PerCpu::new([const { CpuTime::new() }; hyper::cpu::MAX_CPUS]);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CpuTimeSnapshot {
    pub(crate) captured_at_ns: u64,
    pub(crate) reserved_clock: u64,
    pub(crate) online_cpus: u64,
    pub(crate) idle_ns: u64,
    pub(crate) kernel_thread_ns: u64,
    pub(crate) user_thread_ns: u64,
    pub(crate) vcpu_ns: u64,
}

pub(super) fn account_cpu_time(cpu: CpuIndex, role: ThreadRole, elapsed: u64) {
    CPU_TIME[cpu].account(role, elapsed);
}

pub(crate) fn snapshot() -> CpuTimeSnapshot {
    // Include live intervals even when no tick or context switch occurs.
    let _ = super::read_scheduler(|_scheduler| super::state::account_all_cpus());
    let mut snapshot = CpuTimeSnapshot {
        reserved_clock: 0,
        online_cpus: crate::cpu::online_cpu_count() as u64,
        ..CpuTimeSnapshot::default()
    };
    for index in 0..hyper::cpu::MAX_CPUS {
        let Some(cpu) = CpuIndex::new(index) else {
            break;
        };
        snapshot.idle_ns = snapshot
            .idle_ns
            .saturating_add(CPU_TIME[cpu].idle.load(Ordering::Relaxed));
        snapshot.kernel_thread_ns = snapshot
            .kernel_thread_ns
            .saturating_add(CPU_TIME[cpu].kernel_thread.load(Ordering::Relaxed));
        snapshot.user_thread_ns = snapshot
            .user_thread_ns
            .saturating_add(CPU_TIME[cpu].user_thread.load(Ordering::Relaxed));
        snapshot.vcpu_ns = snapshot
            .vcpu_ns
            .saturating_add(CPU_TIME[cpu].vcpu.load(Ordering::Relaxed));
    }
    // Timestamp the completion of the weakly consistent scan. Consumers may
    // use this value as a lower bound for the next sampling deadline.
    snapshot.captured_at_ns = crate::time::monotonic_nanoseconds().unwrap_or(0);
    snapshot
}
