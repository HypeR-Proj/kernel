// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Concurrent admission beyond the former global limit, retirement and reuse.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::diagnostics::log::console;
use crate::sync::Semaphore;
use {crate::task::scheduler, crate::task::scheduler::CpuMask};

const CREATORS: usize = 4;
const PER_CREATOR: usize = 300;
const WORKERS: usize = CREATORS * PER_CREATOR;
static ENTERED: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);
static WORKER_GATE: Semaphore = Semaphore::new(0);
static CREATOR_GATE: Semaphore = Semaphore::new(0);

pub(super) fn run() -> Result<(), &'static str> {
    super::support::quiesce_workers().map_err(|_| "initial quiescence")?;
    let mut previous_slots = None;
    for _ in 0..2 {
        ENTERED.store(0, Ordering::Release);
        FAILED.store(false, Ordering::Release);
        for index in 0..CREATORS {
            let cpu = CpuIndex::new(index % crate::cpu::online_cpu_count()).ok_or("creator CPU")?;
            let id = scheduler::kthread_create_with_affinity(
                "thread-growth/create",
                creator,
                0,
                CpuMask::single(cpu),
            )
            .map_err(|_| "creator allocation")?;
            scheduler::thread_ready(id).map_err(|_| "creator publication")?;
        }
        let ready = crate::task::wait_for_test_progress(30_000_000_000, || {
            Ok::<_, crate::task::SleepError>(
                FAILED.load(Ordering::Acquire)
                    || (ENTERED.load(Ordering::Acquire) == WORKERS
                        && WORKER_GATE.waiter_count() == Ok(WORKERS)
                        && CREATOR_GATE.waiter_count() == Ok(CREATORS)),
            )
        })
        .map_err(|_| "growth progress")?;
        if FAILED.load(Ordering::Acquire) {
            return Err("growth worker failed");
        }
        if !ready {
            return Err("growth timed out");
        }
        if console::active_flush_barrier_count_for_test() != WORKERS {
            return Err("flush registration lost across directory growth");
        }
        let slots = scheduler::registry_slot_count().map_err(|_| "registry observation")?;
        if let Some(previous) = previous_slots
            && slots != previous
        {
            return Err("retired registry slots were not reused");
        }
        previous_slots = Some(slots);
        for _ in 0..WORKERS {
            WORKER_GATE.release().map_err(|_| "worker release")?;
        }
        for _ in 0..CREATORS {
            CREATOR_GATE.release().map_err(|_| "creator release")?;
        }
        super::support::quiesce_workers().map_err(|_| "retirement did not finish")?;
        if FAILED.load(Ordering::Acquire) || console::active_flush_barrier_count_for_test() != 0 {
            return Err("flush owner or worker leaked");
        }
    }
    crate::pr_info!(
        "HypeR test: 1200 concurrent Threads, dynamic flush registrations and slot reuse passed"
    );
    Ok(())
}

extern "C" fn creator(_: usize) {
    for _ in 0..PER_CREATOR {
        let result = scheduler::kthread_create("thread-growth/worker", worker, 0)
            .and_then(scheduler::thread_ready);
        if result != Ok(true) {
            FAILED.store(true, Ordering::Release);
            return;
        }
    }
    if CREATOR_GATE.acquire().is_err() {
        FAILED.store(true, Ordering::Release);
    }
}

extern "C" fn worker(_: usize) {
    let Ok(barrier) = console::register_pending_flush_barrier_for_test() else {
        FAILED.store(true, Ordering::Release);
        return;
    };
    ENTERED.fetch_add(1, Ordering::Release);
    if WORKER_GATE.acquire().is_err() {
        FAILED.store(true, Ordering::Release);
    }
    drop(barrier);
}
