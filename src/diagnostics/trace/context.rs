// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Observational identities and the executing Thread, without registry lookups.

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(2);
static ACTORS: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);

/// Even IDs reserve the adjacent odd ID for a Channel's other receive queue.
/// Exhaustion disables correlation, never an IPC operation or resource reuse.
pub(crate) fn identity() -> u64 {
    NEXT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(2))
        .unwrap_or(0)
}

/// Publish from the incoming switch tail, while local IRQs remain masked.
/// Scheduler selection precedes the actual stack switch and is too early.
pub(crate) fn publish_actor(cpu: CpuIndex, thread: u64) {
    // Only the executing CPU observes this scalar; it grants no authority and
    // publishes no referenced memory. A zero slot denotes an unobserved actor.
    ACTORS[cpu].store(thread, Ordering::Relaxed);
}

/// Read only inside a tracepoint payload, whose machine adapter pins this CPU.
pub(crate) fn actor() -> u64 {
    crate::cpu::current_index().map_or(0, |cpu| ACTORS[cpu].load(Ordering::Relaxed))
}
