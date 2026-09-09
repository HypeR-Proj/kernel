// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real SMP park/wake progress while another CPU retains its reader lane.

use hyper::cpu::CpuIndex;
use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::kernel::sync::Semaphore;
use crate::kernel::task::scheduler::{self, CpuMask};

const ROUNDS: usize = 256;
const TIMEOUT_US: u64 = 5_000_000;
static START: AtomicBool = AtomicBool::new(false);
static HELD: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicUsize = AtomicUsize::new(0);
static PING: Semaphore = Semaphore::new(0);
static PONG: Semaphore = Semaphore::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Scheduler(scheduler::Error),
    Progress,
    Quiescence(super::support::QuiescenceError),
}

pub(super) fn run() -> Result<(), Error> {
    if crate::kernel::cpu::online_cpu_count() < 4 {
        return Ok(());
    }
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    let entries: [extern "C" fn(usize); 3] = [holder, ping, pong];
    for (index, entry) in entries.into_iter().enumerate() {
        let cpu = CpuIndex::new(index + 1).ok_or(Error::Progress)?;
        let id = scheduler::kthread_create_with_affinity(
            "scheduler/parallel",
            entry,
            0,
            CpuMask::single(cpu),
        )
        .map_err(Error::Scheduler)?;
        scheduler::thread_ready(id).map_err(Error::Scheduler)?;
    }
    // All ownership publications precede the deliberately held reader lane.
    START.store(true, Ordering::Release);
    let start = crate::kernel::time::monotonic_microseconds();
    // Require both continuations to actually park before starting the ring.
    while PING.waiter_count() != Ok(1) || PONG.waiter_count() != Ok(1) {
        if crate::kernel::time::monotonic_microseconds().saturating_sub(start) > TIMEOUT_US {
            FAILED.store(true, Ordering::Release);
            RELEASE.store(true, Ordering::Release);
            return Err(Error::Progress);
        }
        core::hint::spin_loop();
    }
    if PING.release().is_err() {
        RELEASE.store(true, Ordering::Release);
        return Err(Error::Progress);
    }
    while DONE.load(Ordering::Acquire) != 2 && !FAILED.load(Ordering::Acquire) {
        if crate::kernel::time::monotonic_microseconds().saturating_sub(start) > TIMEOUT_US {
            FAILED.store(true, Ordering::Release);
            break;
        }
        core::hint::spin_loop();
    }
    RELEASE.store(true, Ordering::Release);
    if FAILED.load(Ordering::Acquire) {
        return Err(Error::Progress);
    }
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    crate::pr_info!("HypeR test: independent CPU wait/wake progress passed (256 round trips)");
    Ok(())
}

extern "C" fn holder(_: usize) {
    while !START.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    if scheduler::hold_reader_for_test(|| {
        HELD.store(true, Ordering::Release);
        let start = crate::kernel::time::monotonic_microseconds();
        while !RELEASE.load(Ordering::Acquire) {
            if crate::kernel::time::monotonic_microseconds().saturating_sub(start) > TIMEOUT_US {
                FAILED.store(true, Ordering::Release);
                break;
            }
            core::hint::spin_loop();
        }
    })
    .is_err()
    {
        FAILED.store(true, Ordering::Release);
    }
}

extern "C" fn ping(_: usize) {
    exchange(&PING, &PONG);
}
extern "C" fn pong(_: usize) {
    exchange(&PONG, &PING);
}

fn exchange(input: &Semaphore, output: &Semaphore) {
    while !HELD.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    for _ in 0..ROUNDS {
        if input.acquire().is_err() || output.release().is_err() {
            FAILED.store(true, Ordering::Release);
            break;
        }
    }
    DONE.fetch_add(1, Ordering::Release);
    // Exit is intentionally outside the retained registry read epoch.
    while !RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}
