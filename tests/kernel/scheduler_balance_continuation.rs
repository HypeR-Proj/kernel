// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A single enqueue must reach candidates beyond the first bounded scan.
//!
//! The FIFO observer never yields or blocks during observation. The worker can
//! therefore run only after the balance IRQ continuation migrates it to CPU 1.
//! This case requires `AArch64`'s actual IRQ-tail scheduler continuation.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::Error;
use crate::task::policy::ThreadPriority;
use crate::task::thread::ThreadId;
use {crate::task, crate::time};
use {crate::task::scheduler, crate::task::scheduler::CpuMask};
use {
    crate::task::scheduling_authority::SCHEDULING_RIGHTS,
    crate::task::scheduling_authority::SchedulingAuthority,
    crate::task::scheduling_authority::SchedulingLimits,
};

const DORMANT_THREADS: usize = 66;
const FIRST_SCAN_SLOTS: usize = 64;
static RELEASE: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static EXPECTED: AtomicU64 = AtomicU64::new(0);
static OBSERVED_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

pub(super) fn run() -> Result<(), Error> {
    if crate::cpu::online_cpu_count() < 2 {
        return Ok(());
    }
    super::super::support::quiesce_workers()?;
    let remote = CpuIndex::new(1).ok_or(Error::Contract("missing continuation target CPU"))?;
    let observer = scheduler::current_thread_id()?;
    let original = scheduler::scheduling_snapshot(observer)?;
    RELEASE.store(false, Ordering::Release);
    FAILED.store(false, Ordering::Release);
    OBSERVED_CPU.store(usize::MAX, Ordering::Release);

    let mut threads: [Option<(ThreadId, bool)>; DORMANT_THREADS] = [None; DORMANT_THREADS];
    let mut result = (|| {
        scheduler::set_thread_fifo_policy(observer, ThreadPriority::HIGHEST)?;
        let mut candidate = None;
        for (index, record) in threads.iter_mut().enumerate() {
            let id = scheduler::kthread_create_with_affinity(
                "balance/continuation",
                worker,
                0,
                CpuMask::single(CpuIndex::BOOT),
            )?;
            *record = Some((id, false));
            let slot = scheduler::thread_registry_slot_for_test(id)
                .ok_or(Error::Contract("test worker has no registry slot"))?;
            // Reused registry slots are LIFO. Choose the highest actual slot,
            // rather than assuming the last allocation is beyond slot 64.
            if candidate.is_none_or(|(_, _, previous)| slot > previous) {
                candidate = Some((index, id, slot));
            }
        }
        let (index, id, slot) = candidate.ok_or(Error::Contract("no continuation candidate"))?;
        if slot < FIRST_SCAN_SLOTS {
            return Err(Error::Contract(
                "candidate fits within the first balance scan",
            ));
        }
        let affinity = CpuMask::single(CpuIndex::BOOT).with_cpu(remote);
        scheduler::set_thread_affinity(id, affinity)?;
        if scheduler::thread_placement(id)? != (CpuIndex::BOOT, affinity) {
            return Err(Error::Contract("dormant candidate was already migrated"));
        }
        spin_wait(
            "earlier balance requests did not drain",
            scheduler::balance_idle_for_test,
        )?;
        EXPECTED.store(id.get(), Ordering::Release);

        // This is the sole test-originated readiness/balance event. During
        // observation there are no scheduler reads, yields, timer waits, or
        // repeated request calls that could hide a lost continuation prompt.
        let mut started = false;
        let observed = observe_from_deep_stack(id, &mut started);
        threads[index] = Some((id, started));
        observed?;
        if FAILED.load(Ordering::Acquire) || OBSERVED_CPU.load(Ordering::Acquire) != remote.get() {
            return Err(Error::Contract(
                "tail candidate did not execute on its migration target",
            ));
        }
        Ok(())
    })();

    // Every failure path releases an already-started worker, discards every
    // dormant filler, restores the observer, and waits for real retirement.
    RELEASE.store(true, Ordering::Release);
    for (id, started) in threads.into_iter().flatten() {
        if !started {
            let cleanup = scheduler::discard_dormant_kernel_thread(id).map_err(Error::from);
            if result.is_ok() {
                result = cleanup;
            }
        }
    }
    let restore = scheduler::configure_scheduling(
        observer,
        original,
        SchedulingAuthority::new(SCHEDULING_RIGHTS),
        SchedulingLimits::SYSTEM,
    )
    .map_err(Error::from);
    if result.is_ok() {
        result = restore;
    }
    let quiescence = super::super::support::quiesce_workers().map_err(Error::from);
    result?;
    quiescence?;
    crate::pr_info!(
        "HypeR test: deep-stack single-event balance continuation beyond 64 registry slots passed"
    );
    Ok(())
}

/// Keep a syscall-sized frame live through enqueue and its IRQ-tail migration.
/// Unlike a sampled watermark, this forces overlap with the balancing path.
#[inline(never)]
fn observe_from_deep_stack(id: ThreadId, started: &mut bool) -> Result<(), Error> {
    let mut frame = [0u8; 8 * 1024];
    for (index, byte) in frame.iter_mut().enumerate() {
        // SAFETY: every byte belongs to the exclusively borrowed local array.
        unsafe { core::ptr::write_volatile(byte, index as u8) };
    }
    core::hint::black_box(&frame);
    scheduler::thread_ready(id)?;
    *started = true;
    let result = spin_wait(
        "balance continuation did not reach the tail candidate",
        || OBSERVED_CPU.load(Ordering::Acquire) != usize::MAX || FAILED.load(Ordering::Acquire),
    );
    core::hint::black_box(&frame);
    if frame
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte != index as u8)
    {
        return Err(Error::Contract("balancing corrupted its caller's stack"));
    }
    result
}

fn spin_wait(failure: &'static str, condition: impl FnMut() -> bool) -> Result<(), Error> {
    if time::spin_wait_until(task::TEST_PROGRESS_TIMEOUT_NS, condition)
        .map_err(task::SleepError::from)?
    {
        Ok(())
    } else {
        Err(Error::Contract(failure))
    }
}

extern "C" fn worker(_: usize) {
    if scheduler::current_thread_id().map(ThreadId::get) != Ok(EXPECTED.load(Ordering::Acquire)) {
        FAILED.store(true, Ordering::Release);
        return;
    }
    let Some(cpu) = crate::cpu::current_index() else {
        FAILED.store(true, Ordering::Release);
        return;
    };
    OBSERVED_CPU.store(cpu.get(), Ordering::Release);
    while !RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}
