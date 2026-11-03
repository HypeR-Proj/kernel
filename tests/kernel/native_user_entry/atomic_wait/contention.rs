// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real same-address Process waits; report bucket work separately from correctness.

use super::*;
use core::sync::atomic::AtomicU32;

const PROCESSES: usize = 8;
static OWNERS: InterruptSpinLock<[Option<Process>; PROCESSES], crate::hal::irq::LocalMask> =
    InterruptSpinLock::new([const { None }; PROCESSES]);
static CANCELLED: AtomicBool = AtomicBool::new(false);
static COMPLETED: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);
static READY: AtomicUsize = AtomicUsize::new(0);
static START: AtomicBool = AtomicBool::new(false);

fn owner(index: usize) -> Result<Process, Error> {
    OWNERS
        .with(|owners| owners[index].clone())
        .ok_or(Error::AtomicWait("missing contention owner"))
}

pub(super) fn run(domain: &ResourceDomain, group: &TaskGroup, program: &[u8]) -> Result<(), Error> {
    CANCELLED.store(false, Ordering::Release);
    COMPLETED.store(0, Ordering::Release);
    FAILED.store(false, Ordering::Release);
    let result = exercise(domain, group, program);
    CANCELLED.store(true, Ordering::Release);
    let mut cleanup = Ok(());
    for index in 0..PROCESSES {
        if let Ok(process) = owner(index) {
            let woke = atomic_wait::wake(&process, ADDRESS, u32::MAX)
                .map(|_| ())
                .map_err(|_| Error::AtomicWait("contention cleanup wake"));
            cleanup = cleanup.and(woke);
        }
    }
    // Static owners stay alive even if quiescence fails, so no worker can borrow
    // a freed fixture. The fail-fast harness then reports the failed cleanup.
    crate::kernel_tests::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    for index in 0..PROCESSES {
        if let Some(process) = OWNERS.with(|owners| owners[index].take()) {
            process.request_stop(TerminalReason::Requested);
            cleanup = cleanup.and(retire_process(&process));
        }
    }
    result.and(cleanup)
}

fn exercise(domain: &ResourceDomain, group: &TaskGroup, program: &[u8]) -> Result<(), Error> {
    let mut buckets = [0; PROCESSES];
    for (index, bucket) in buckets.iter_mut().enumerate() {
        let process = prepare_process(domain, group, program, MachineAbi::Aarch64)?;
        // Retain ownership before any later fallible step, including worker
        // creation, so the common cleanup can stop and retire this Process.
        OWNERS.with(|owners| owners[index] = Some(process.clone()));
        *bucket = atomic_wait::bucket_index_for_test(&process, ADDRESS)
            .map_err(|_| Error::AtomicWait("contention bucket"))?;
        let id = scheduler::kthread_create("atomic-wait/process", worker, index)
            .map_err(|_| Error::Scheduler)?;
        if !matches!(scheduler::thread_ready(id), Ok(true)) {
            scheduler::discard_dormant_kernel_thread(id).map_err(|_| Error::Scheduler)?;
            return Err(Error::Scheduler);
        }
    }
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        for index in 0..PROCESSES {
            if atomic_wait::waiter_count(&owner(index)?, ADDRESS)
                .map_err(|_| Error::AtomicWait("contention publication"))?
                != 1
            {
                return Ok::<_, Error>(false);
            }
        }
        Ok(true)
    })? {
        return Err(Error::AtomicWait("contention publication deadline"));
    }
    let scanned = atomic_wait::scanned_waiters_for_test();
    let started =
        crate::time::monotonic_nanoseconds().map_err(|_| Error::AtomicWait("contention clock"))?;
    for index in 0..PROCESSES {
        if atomic_wait::wake(&owner(index)?, ADDRESS, u32::MAX)
            .map_err(|_| Error::AtomicWait("contention wake"))?
            != 1
        {
            return Err(Error::AtomicWait("cross-Process wake isolation"));
        }
        if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
            Ok::<_, Error>(COMPLETED.load(Ordering::Acquire) == index + 1)
        })? {
            return Err(Error::AtomicWait("contention completion deadline"));
        }
        for other in index + 1..PROCESSES {
            if atomic_wait::waiter_count(&owner(other)?, ADDRESS)
                .map_err(|_| Error::AtomicWait("other Process registration"))?
                != 1
            {
                return Err(Error::AtomicWait("other Process woke early"));
            }
        }
    }
    if FAILED.load(Ordering::Acquire) {
        return Err(Error::AtomicWait("contention worker result"));
    }
    let elapsed = crate::time::monotonic_nanoseconds()
        .map_err(|_| Error::AtomicWait("contention clock"))?
        .saturating_sub(started);
    let visited = atomic_wait::scanned_waiters_for_test().wrapping_sub(scanned);
    let occupied = buckets
        .iter()
        .enumerate()
        .filter(|(index, bucket)| !buckets[..*index].contains(bucket))
        .count();
    crate::pr_info!(
        "HypeR test: atomic same-offset processes={} buckets={} wake_scans={} elapsed_ns={}",
        PROCESSES,
        occupied,
        visited,
        elapsed
    );
    crate::kernel_tests::support::quiesce_workers().map_err(|_| Error::Scheduler)?;
    ping_pong(domain)
}

const ROUNDS: u32 = 32;

fn ping_pong(domain: &ResourceDomain) -> Result<(), Error> {
    let baseline = domain.usage();
    COMPLETED.store(0, Ordering::Release);
    READY.store(0, Ordering::Release);
    START.store(false, Ordering::Release);
    for index in 0..PROCESSES * 2 {
        let id = scheduler::kthread_create("atomic-wait/ping-pong", ping_pong_worker, index)
            .map_err(|_| Error::Scheduler)?;
        if !matches!(scheduler::thread_ready(id), Ok(true)) {
            scheduler::discard_dormant_kernel_thread(id).map_err(|_| Error::Scheduler)?;
            return Err(Error::Scheduler);
        }
    }
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(
            READY.load(Ordering::Acquire) == PROCESSES * 2 || FAILED.load(Ordering::Acquire),
        )
    })? || FAILED.load(Ordering::Acquire)
    {
        return Err(Error::AtomicWait("ping-pong startup"));
    }
    let scanned = atomic_wait::scanned_waiters_for_test();
    let started =
        crate::time::monotonic_nanoseconds().map_err(|_| Error::AtomicWait("ping-pong clock"))?;
    START.store(true, Ordering::Release);
    if !wait_for_test_progress(TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(COMPLETED.load(Ordering::Acquire) == PROCESSES * 2)
    })? || FAILED.load(Ordering::Acquire)
    {
        return Err(Error::AtomicWait("ping-pong completion"));
    }
    let elapsed = crate::time::monotonic_nanoseconds()
        .map_err(|_| Error::AtomicWait("ping-pong clock"))?
        .saturating_sub(started);
    if domain.usage() != baseline {
        return Err(Error::AtomicWait("ping-pong resource retirement"));
    }
    crate::pr_info!(
        "HypeR test: atomic same-offset contention processes={} turns={} wake_scans={} elapsed_ns={}",
        PROCESSES,
        PROCESSES * 2 * ROUNDS as usize,
        atomic_wait::scanned_waiters_for_test().wrapping_sub(scanned),
        elapsed
    );
    Ok(())
}

fn exchange_turns(index: usize) -> Result<(), Error> {
    let process = owner(index / 2)?;
    let pin = process
        .address_space_owner()
        .map_err(|_| Error::AddressSpace)?
        .pin_atomic_u32(crate::mm::user_space::UserAddress::new(ADDRESS))
        .map_err(|_| Error::AtomicWait("ping-pong backing"))?;
    let linear = crate::mm::memory::linear_address(pin.physical)
        .ok_or(Error::AtomicWait("ping-pong linear address"))?;
    // SAFETY: pin retains this aligned, writable u32's resident backing until
    // the function returns. After initialization all concurrent accesses use
    // AtomicU32, including the production wait predicate check.
    let word = unsafe { &*core::ptr::with_exposed_provenance::<AtomicU32>(linear) };
    READY.fetch_add(1, Ordering::Release);
    while !START.load(Ordering::Acquire) {
        if CANCELLED.load(Ordering::Acquire) {
            return Ok(());
        }
        scheduler::yield_now().map_err(|_| Error::Scheduler)?;
    }
    for round in 0..ROUNDS {
        let expected = round * 2 + (index % 2) as u32;
        loop {
            if CANCELLED.load(Ordering::Acquire) {
                return Ok(());
            }
            let observed = word.load(Ordering::Acquire);
            if observed == expected {
                break;
            }
            if observed > expected {
                return Err(Error::AtomicWait("ping-pong skipped turn"));
            }
            atomic_wait::wait(&process, ADDRESS, observed, u64::MAX, || {
                CANCELLED.load(Ordering::Acquire)
            })
            .map_err(|_| Error::AtomicWait("ping-pong wait"))?;
        }
        word.store(expected + 1, Ordering::Release);
        if atomic_wait::wake(&process, ADDRESS, u32::MAX)
            .map_err(|_| Error::AtomicWait("ping-pong wake"))?
            > 1
        {
            return Err(Error::AtomicWait("ping-pong woke unrelated waiter"));
        }
    }
    Ok(())
}

extern "C" fn ping_pong_worker(index: usize) {
    if exchange_turns(index).is_err() {
        FAILED.store(true, Ordering::Release);
    }
    COMPLETED.fetch_add(1, Ordering::Release);
}

extern "C" fn worker(index: usize) {
    let result = owner(index).and_then(|process| {
        atomic_wait::wait(&process, ADDRESS, 0, u64::MAX, || {
            CANCELLED.load(Ordering::Acquire)
        })
        .map_err(|_| Error::AtomicWait("contention worker wait"))
    });
    match result {
        Ok(WaitOutcome::Notified) => {}
        Ok(WaitOutcome::Cancelled) if CANCELLED.load(Ordering::Acquire) => {}
        _ => FAILED.store(true, Ordering::Release),
    }
    COMPLETED.fetch_add(1, Ordering::Release);
}
