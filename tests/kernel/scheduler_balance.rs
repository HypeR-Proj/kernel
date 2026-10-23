// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Actual execution placement after a wake burst, independent of creation policy.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler::{self, CpuMask};
use crate::kernel::task::thread::ThreadId;

#[cfg(target_arch = "aarch64")]
#[path = "scheduler_balance_continuation.rs"]
mod continuation;

const WORKERS: usize = 4;
const MIN_ROUNDS: usize = 32;
static GATE: Semaphore = Semaphore::new(0);
static RELEASE: AtomicBool = AtomicBool::new(false);
static FAILURE: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_CPUS: AtomicUsize = AtomicUsize::new(0);
static EXPECTED: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
static ENTERED: [AtomicBool; WORKERS] = [const { AtomicBool::new(false) }; WORKERS];
static PROGRESS: [AtomicUsize; WORKERS] = [const { AtomicUsize::new(0) }; WORKERS];

pub(super) enum Error {
    Scheduler(scheduler::Error),
    Sleep(crate::kernel::task::SleepError),
    Synchronization(crate::kernel::sync::Error),
    Quiescence(super::support::QuiescenceError),
    Contract(&'static str),
    Worker(usize),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scheduler(error) => f.debug_tuple("Scheduler").field(error).finish(),
            Self::Sleep(error) => f.debug_tuple("Sleep").field(error).finish(),
            Self::Synchronization(error) => f.debug_tuple("Synchronization").field(error).finish(),
            Self::Quiescence(error) => f.debug_tuple("Quiescence").field(error).finish(),
            Self::Contract(error) => f.debug_tuple("Contract").field(error).finish(),
            Self::Worker(error) => f.debug_tuple("Worker").field(error).finish(),
        }
    }
}

impl From<scheduler::Error> for Error {
    fn from(value: scheduler::Error) -> Self {
        Self::Scheduler(value)
    }
}
impl From<crate::kernel::task::SleepError> for Error {
    fn from(value: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(value)
    }
}
impl From<crate::kernel::sync::Error> for Error {
    fn from(value: crate::kernel::sync::Error) -> Self {
        Self::Synchronization(value)
    }
}
impl From<super::support::QuiescenceError> for Error {
    fn from(value: super::support::QuiescenceError) -> Self {
        Self::Quiescence(value)
    }
}

pub(super) fn run() -> Result<(), Error> {
    exercise(false)?;
    exercise(true)?;
    #[cfg(target_arch = "aarch64")]
    continuation::run()?;
    crate::pr_info!("HypeR test: runnable balancing, wake bursts and affinity confinement passed");
    Ok(())
}

fn exercise(widen: bool) -> Result<(), Error> {
    super::support::quiesce_workers()?;
    RELEASE.store(false, Ordering::Release);
    FAILURE.store(0, Ordering::Release);
    OBSERVED_CPUS.store(0, Ordering::Release);
    for index in 0..WORKERS {
        ENTERED[index].store(false, Ordering::Release);
        PROGRESS[index].store(0, Ordering::Release);
    }
    let mut threads: [Option<ThreadId>; WORKERS] = [None; WORKERS];
    let result = (|| {
        for (index, slot) in threads.iter_mut().enumerate() {
            let id = scheduler::kthread_create_with_affinity(
                "balance/wake",
                worker,
                index,
                CpuMask::single(CpuIndex::BOOT),
            )?;
            EXPECTED[index].store(id.get(), Ordering::Release);
            if let Err(error) = scheduler::thread_ready(id) {
                scheduler::discard_dormant_kernel_thread(id)?;
                return Err(error.into());
            }
            *slot = Some(id);
        }
        wait("workers did not park", || {
            Ok(GATE.waiter_count()? == WORKERS)
        })?;
        let remote = (crate::kernel::cpu::online_cpu_count() > 1)
            .then(|| CpuIndex::new(1))
            .flatten();
        let affinity = if widen {
            remote.map_or(CpuMask::single(CpuIndex::BOOT), |cpu| {
                CpuMask::single(CpuIndex::BOOT).with_cpu(cpu)
            })
        } else {
            CpuMask::single(CpuIndex::BOOT)
        };
        for id in threads.into_iter().flatten() {
            // All workers really ran and parked on CPU 0. Merely distributing
            // newly created Threads cannot make this case pass.
            scheduler::set_thread_affinity(id, affinity)?;
            if scheduler::thread_placement(id)? != (CpuIndex::BOOT, affinity) {
                return Err(Error::Contract(
                    "blocked placement changed without runnable work",
                ));
            }
        }
        for _ in 0..WORKERS {
            GATE.release()?;
        }
        let required = if widen && remote.is_some() { 0b11 } else { 1 };
        wait("runnable cohort did not make balanced progress", || {
            check_failure()?;
            let mut on_remote = 0;
            for id in threads.into_iter().flatten() {
                if scheduler::thread_placement(id)?.0 != CpuIndex::BOOT {
                    on_remote += 1;
                }
            }
            // Four workers must settle 2:2, not merely become runnable on two
            // CPUs. This distinguishes event-driven correction of a busy 3:1
            // split from a one-off transfer when a target was idle.
            let expected_remote = if required == 0b11 { WORKERS / 2 } else { 0 };
            Ok(OBSERVED_CPUS.load(Ordering::Acquire) == required
                && on_remote == expected_remote
                && PROGRESS
                    .iter()
                    .all(|progress| progress.load(Ordering::Acquire) >= MIN_ROUNDS))
        })?;
        // Keep both cases runnable over repeated scheduling events, including
        // single-CPU boots, rather than observing only the first dispatch.
        crate::kernel::task::sleep_ms(80)?;
        check_failure()?;
        if OBSERVED_CPUS.load(Ordering::Acquire) != required {
            return Err(Error::Contract("execution escaped the allowed CPUs"));
        }
        Ok(())
    })();

    // Static records stay alive through every failure path. Release workers
    // still waiting at the gate before waiting for scheduler retirement.
    RELEASE.store(true, Ordering::Release);
    for _ in threads.into_iter().flatten() {
        GATE.release()?;
    }
    super::support::quiesce_workers()?;
    while GATE.try_acquire() {}
    result
}

fn wait(
    failure: &'static str,
    condition: impl FnMut() -> Result<bool, Error>,
) -> Result<(), Error> {
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        condition,
    )? {
        Ok(())
    } else {
        Err(Error::Contract(failure))
    }
}

fn check_failure() -> Result<(), Error> {
    match FAILURE.load(Ordering::Acquire) {
        0 => Ok(()),
        code => Err(Error::Worker(code)),
    }
}

fn fail(code: usize) {
    let _ = FAILURE.compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
}

extern "C" fn worker(index: usize) {
    let Some(entered) = ENTERED.get(index) else {
        fail(1);
        return;
    };
    if entered.swap(true, Ordering::AcqRel) {
        fail(2);
        return;
    }
    if GATE.acquire().is_err() {
        fail(3);
        return;
    }
    let mut rounds = 0usize;
    while !RELEASE.load(Ordering::Acquire) {
        if scheduler::current_thread_id().map(ThreadId::get)
            != Ok(EXPECTED[index].load(Ordering::Acquire))
        {
            fail(4);
            return;
        }
        let Some(cpu) = crate::kernel::cpu::current_index() else {
            fail(5);
            return;
        };
        if cpu.get() > 1 {
            fail(6);
            return;
        }
        OBSERVED_CPUS.fetch_or(1 << cpu.get(), Ordering::AcqRel);
        let next = rounds.wrapping_add(1);
        if PROGRESS[index]
            .compare_exchange(rounds, next, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            fail(7);
            return;
        }
        rounds = next;
        if scheduler::yield_now().is_err() {
            fail(8);
            return;
        }
    }
}
