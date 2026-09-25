// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Thread-owned waiter lifetime across collision, migration, timeout and cancellation.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hyper::cpu::CpuIndex;
use hyper::mm::PAGE_SIZE;
use hyper::sync::InterruptSpinLock;

use super::{Error, IMAGE_BASE, prepare_process, retire_process};
use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::process::{MachineAbi, Process, TaskGroup, TerminalReason, atomic_wait};
use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::task::{TEST_PROGRESS_TIMEOUT_NS, WaitOutcome, wait_for_test_progress};

const ADDRESS: u64 = IMAGE_BASE + PAGE_SIZE * 2;
const COLLISION: u64 = ADDRESS + 256;
const WORKERS: usize = 8;
static PROCESS: InterruptSpinLock<Option<Process>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);
static CANCEL: AtomicBool = AtomicBool::new(false);
static DONE: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);

pub(super) fn run(domain: &ResourceDomain, group: &TaskGroup, program: &[u8]) -> Result<(), Error> {
    // Isolate accounting from deferred reclamation of earlier Native probes.
    let domain = domain
        .try_new_child(ResourceLimits::UNLIMITED)
        .map_err(|_| Error::Construction)?;
    let process = prepare_process(&domain, group, program, MachineAbi::Aarch64)?;
    PROCESS.with(|slot| {
        if slot.is_some() {
            hyper::debug::invariant_failure("atomic wait test already active");
        }
        *slot = Some(process.clone());
    });
    CANCEL.store(false, Ordering::Release);
    DONE.store(0, Ordering::Release);
    FAILED.store(false, Ordering::Release);
    let result = exercise(&process);
    // A failed assertion must not leave a worker borrowing a retired mapping.
    // Cancellation before publication and wake under the bucket lock jointly
    // cover workers which have not entered wait() yet, as well as parked ones.
    CANCEL.store(true, Ordering::Release);
    for address in [ADDRESS, COLLISION] {
        atomic_wait::wake(&process, address, u32::MAX)
            .map_err(|_| Error::AtomicWait("cleanup wake"))?;
    }
    super::super::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    let retained = PROCESS.with(Option::take);
    drop(retained);
    process.request_stop(TerminalReason::Requested);
    retire_process(&process)?;
    result?;
    crate::pr_info!(
        "HypeR test: Thread-owned atomic waiters retired after collision, migration and timeout"
    );
    Ok(())
}

/// Completing arbitration must not permit a new wait or exit until every
/// signal subscription and timeout has retired, in either cleanup order.
fn exercise_source_cleanup() -> Result<(), Error> {
    use crate::kernel::task::{WaitMobility, WaitSource, WaitSourceRegistration};
    for timer_first in [false, true] {
        let registration =
            scheduler::begin_wait(WaitMobility::Migratable).map_err(|_| Error::Scheduler)?;
        let ticket = registration.ticket();
        let first = WaitSourceRegistration::new(ticket, WaitSource::Signal);
        let second = WaitSourceRegistration::new(ticket, WaitSource::Signal);
        let timer = WaitSourceRegistration::new(ticket, WaitSource::Timer);
        scheduler::resolve_wait(ticket, WaitOutcome::Cancelled).map_err(|_| Error::Scheduler)?;
        scheduler::finish_wait(registration).map_err(|_| Error::Scheduler)?;
        verify_cleanup_blocks_reuse()?;
        drop(first);
        verify_cleanup_blocks_reuse()?;
        if timer_first {
            drop(timer);
            verify_cleanup_blocks_reuse()?;
            drop(second);
        } else {
            drop(second);
            verify_cleanup_blocks_reuse()?;
            drop(timer);
        }
        let next = scheduler::begin_wait(WaitMobility::Migratable)
            .map_err(|_| Error::AtomicWait("cleanup did not allow reuse"))?;
        if next.ticket() == ticket {
            return Err(Error::AtomicWait("wait generation reused"));
        }
        scheduler::resolve_wait(next.ticket(), WaitOutcome::Cancelled)
            .map_err(|_| Error::Scheduler)?;
        scheduler::finish_wait(next).map_err(|_| Error::Scheduler)?;
    }
    Ok(())
}

fn verify_cleanup_blocks_reuse() -> Result<(), Error> {
    match scheduler::begin_wait(crate::kernel::task::WaitMobility::Migratable) {
        Err(scheduler::Error::InvalidWaitRegistration) => {}
        Err(_) => return Err(Error::Scheduler),
        Ok(registration) => {
            scheduler::resolve_wait(registration.ticket(), WaitOutcome::Cancelled)
                .map_err(|_| Error::Scheduler)?;
            scheduler::finish_wait(registration).map_err(|_| Error::Scheduler)?;
            return Err(Error::AtomicWait("live source allowed wait reuse"));
        }
    }
    if !scheduler::verify_wait_exit_rejected().map_err(|_| Error::Scheduler)? {
        return Err(Error::AtomicWait("live source allowed exit"));
    }
    Ok(())
}

fn exercise(process: &Process) -> Result<(), Error> {
    exercise_source_cleanup()?;
    // The test image's writable stack page starts zero-filled. Warm the backing
    // lease before comparing accounting so fault materialization is excluded.
    if atomic_wait::wait(process, ADDRESS, 1, u64::MAX, || false)
        .map_err(|_| Error::AtomicWait("warm backing"))?
        != WaitOutcome::Notified
    {
        return Err(Error::AtomicWait("mismatch result"));
    }
    let domain = process.resource_domain();
    let baseline = domain.usage();
    if !atomic_wait::verify_exit_guard(process, ADDRESS)
        .map_err(|_| Error::AtomicWait("exit guard"))?
        || domain.usage() != baseline
        || atomic_wait::waiter_count(process, ADDRESS)
            .map_err(|_| Error::AtomicWait("exit guard cleanup"))?
            != 0
    {
        return Err(Error::AtomicWait("linked node allowed Thread exit"));
    }

    for _ in 0..16 {
        let cancelled = atomic_wait::wait(process, ADDRESS, 0, u64::MAX, || true)
            .map_err(|_| Error::AtomicWait("prepublication cancellation"))?;
        let deadline = crate::kernel::time::monotonic_nanoseconds()
            .map_err(|_| Error::AtomicWait("read clock"))?
            .checked_add(10_000_000)
            .ok_or(Error::AtomicWait("deadline overflow"))?;
        let timed_out = atomic_wait::wait(process, ADDRESS, 0, deadline, || false)
            .map_err(|_| Error::AtomicWait("timed wait"))?;
        if cancelled != WaitOutcome::Cancelled
            || timed_out != WaitOutcome::TimedOut
            || atomic_wait::waiter_count(process, ADDRESS)
                .map_err(|_| Error::AtomicWait("registration count"))?
                != 0
            || domain.usage() != baseline
        {
            return Err(Error::AtomicWait("wait retirement accounting"));
        }
    }
    let mut ids = [None; WORKERS];
    for (index, slot) in ids.iter_mut().enumerate() {
        let id = scheduler::kthread_create_with_affinity(
            "atomic-wait/thread",
            worker,
            index,
            CpuMask::single(CpuIndex::BOOT),
        )
        .map_err(|_| Error::Scheduler)?;
        *slot = Some(id);
        scheduler::thread_ready(id).map_err(|_| Error::Scheduler)?;
    }
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>([ADDRESS, COLLISION].into_iter().all(|address| {
            atomic_wait::waiter_count(process, address).is_ok_and(|count| count == WORKERS / 2)
        }))
    })? {
        return Err(Error::AtomicWait("worker publication"));
    }
    let active = domain.usage();
    if active.committed(ResourceKind::KernelMemoryBytes)
        <= baseline.committed(ResourceKind::KernelMemoryBytes)
        || active.committed(ResourceKind::Timers) != baseline.committed(ResourceKind::Timers)
    {
        return Err(Error::AtomicWait("active waiter accounting"));
    }
    if crate::kernel::cpu::online_cpu_count() > 1 {
        for id in ids.into_iter().flatten() {
            scheduler::set_thread_affinity(
                id,
                CpuMask::single(CpuIndex::new(1).ok_or(Error::Scheduler)?),
            )
            .map_err(|_| Error::Scheduler)?;
        }
    }
    // All entries share a bucket, but the exact virtual address must select
    // only one group. This unlinks interior as well as head/tail Thread-owned nodes.
    if atomic_wait::wake(process, ADDRESS, 0).map_err(|_| Error::AtomicWait("zero-count wake"))?
        != 0
        || atomic_wait::wake(process, ADDRESS, 1).map_err(|_| Error::AtomicWait("wake one"))? != 1
    {
        return Err(Error::AtomicWait("wake count"));
    }
    wait_done(1)?;
    if atomic_wait::wake(process, ADDRESS, u32::MAX)
        .map_err(|_| Error::AtomicWait("wake remaining"))?
        != (WORKERS / 2 - 1) as u64
    {
        return Err(Error::AtomicWait("remaining count"));
    }
    wait_done(WORKERS / 2)?;
    if atomic_wait::waiter_count(process, COLLISION)
        .map_err(|_| Error::AtomicWait("collision registration count"))?
        != WORKERS / 2
        || atomic_wait::wake(process, COLLISION, u32::MAX)
            .map_err(|_| Error::AtomicWait("collision wake"))?
            != (WORKERS / 2) as u64
    {
        return Err(Error::AtomicWait("collision isolation"));
    }
    wait_done(WORKERS)?;
    if FAILED.load(Ordering::Acquire) || domain.usage() != baseline {
        return Err(Error::AtomicWait("worker result or accounting"));
    }
    Ok(())
}

fn wait_done(count: usize) -> Result<(), Error> {
    if wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(DONE.load(Ordering::Acquire) == count)
    })? {
        Ok(())
    } else {
        Err(Error::AtomicWait("worker completion"))
    }
}

extern "C" fn worker(index: usize) {
    let process = match PROCESS.with(|slot| slot.clone()) {
        Some(process) => process,
        None => hyper::debug::invariant_failure("atomic wait test missing Process"),
    };
    let address = if index.is_multiple_of(2) {
        ADDRESS
    } else {
        COLLISION
    };
    if !matches!(
        atomic_wait::wait(&process, address, 0, u64::MAX, || {
            CANCEL.load(Ordering::Acquire)
        }),
        Ok(WaitOutcome::Notified)
    ) {
        FAILED.store(true, Ordering::Release);
    }
    let expected_cpu = usize::from(crate::kernel::cpu::online_cpu_count() > 1);
    if !CANCEL.load(Ordering::Acquire)
        && crate::kernel::cpu::current_index().map(CpuIndex::get) != Some(expected_cpu)
    {
        FAILED.store(true, Ordering::Release);
    }
    drop(process);
    DONE.fetch_add(1, Ordering::Release);
}
