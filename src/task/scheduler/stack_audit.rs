// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Opt-in workload evidence, not a bound on every possible stack pointer.
//!
//! Watermarks measure modified bytes; untouched frame reservations can escape
//! measurement. Retired threads include their Native entry and interrupt-tail
//! history, but still-running services and the reaper itself are not sampled.
//! IRQ observations cover only CPUs on which the reaper actually runs. The
//! feature adds scanning and logging overhead and must not measure performance.
//! `samples` is the observation ordinal at a reported maximum, not the final
//! workload sample count: unchanged maxima deliberately produce no log line.

use core::sync::atomic::{AtomicUsize, Ordering};

use {crate::mm::stack, crate::mm::stack::StackStatistics};
use {crate::task::thread::ExecutionKind, crate::task::thread::Thread};

struct Observations {
    samples: AtomicUsize,
    maximum: AtomicUsize,
}

impl Observations {
    const fn new() -> Self {
        Self {
            samples: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        }
    }

    fn record(&self, kind: &str, owner: &str, statistics: StackStatistics) {
        let samples = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = self.maximum.fetch_max(statistics.used, Ordering::Relaxed);
        if samples == 1 || statistics.used > previous || !statistics.canary_intact {
            crate::pr_info!(
                "HypeR STACK-AUDIT kind={} owner={} samples={} used={} remaining={} size={} canary={}",
                kind,
                owner,
                samples,
                statistics.used,
                statistics.remaining,
                statistics.size,
                statistics.canary_intact,
            );
        }
    }
}

static THREADS: [Observations; 3] = [const { Observations::new() }; 3];
static IRQS: [Observations; hyper::cpu::MAX_CPUS] =
    [const { Observations::new() }; hyper::cpu::MAX_CPUS];

/// The sole caller owns a scheduler-detached terminated Thread, after the
/// incoming switch tail has relinquished the outgoing context. No scheduler
/// lock is held while scanning or logging its pinned stack.
pub(super) fn record_retired(thread: &Thread) {
    let (index, kind) = match thread.execution_kind() {
        ExecutionKind::Kernel => (0, "kernel"),
        ExecutionKind::User => (1, "user"),
        ExecutionKind::Vcpu => (2, "vcpu"),
    };
    if let Some(statistics) = thread.kernel_stack_statistics() {
        THREADS[index].record(kind, thread.name_snapshot().as_str(), statistics);
    }

    // This function runs on the ordinary reaper stack. The accessor masks
    // local IRQs and rejects migration between selecting and checking the CPU;
    // a rejected sample is skipped, never replaced with a remote scan.
    if let Some(cpu) = crate::cpu::current_index()
        && let Some((irq, _)) = stack::cpu_stack_statistics(cpu.get())
    {
        let observations = &IRQS[cpu.get()];
        let samples = observations.samples.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = observations.maximum.fetch_max(irq.used, Ordering::Relaxed);
        if samples == 1 || irq.used > previous || !irq.canary_intact {
            crate::pr_info!(
                "HypeR STACK-AUDIT kind=irq cpu={} samples={} used={} remaining={} size={} canary={}",
                cpu.get(),
                samples,
                irq.used,
                irq.remaining,
                irq.size,
                irq.canary_intact,
            );
        }
    }
}
