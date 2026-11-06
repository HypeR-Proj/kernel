// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Broadcast progress and notification cost with competing `ByteChannel` readers.

use alloc::boxed::Box;
use hyper::cpu::CpuIndex;
use hyper::mm::{FallibleArc, try_box};
use hyper::sync::atomic::{AtomicUsize, Ordering};

use crate::accounting::{ResourceDomain, ResourceLimits};
use crate::ipc::{ByteChannel, ByteChannelError, PreparedByteMessage};
use crate::object::{KernelObject, SignalWaitOutcome};
use crate::sync::Semaphore;
use crate::task::scheduler::{self, CpuMask};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Construction,
    Channel(ByteChannelError),
    Progress,
    Scheduler(scheduler::Error),
    Quiescence(super::support::QuiescenceError),
}

impl From<crate::task::SleepError> for Error {
    fn from(_: crate::task::SleepError) -> Self {
        Self::Progress
    }
}

struct Readers {
    domain: ResourceDomain,
    sender: ByteChannel,
    receiver: ByteChannel,
    attempts: AtomicUsize,
    delivered: AtomicUsize,
    failed: AtomicUsize,
    done: Semaphore,
    deadline: u64,
    expect_close: bool,
}

pub(super) fn run() -> Result<(), Error> {
    for count in [1, 8, 17] {
        exercise(count, false, false)?;
        if crate::cpu::online_cpu_count() > 1 {
            exercise(count, true, false)?;
        }
    }
    exercise(17, crate::cpu::online_cpu_count() > 1, true)?;
    Ok(())
}

fn exercise(count: usize, spread: bool, expect_close: bool) -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let (sender, receiver) = ByteChannel::try_pair(&domain).map_err(Error::Channel)?;
    let state = FallibleArc::try_new(Readers {
        domain,
        sender,
        receiver,
        attempts: AtomicUsize::new(0),
        delivered: AtomicUsize::new(0),
        failed: AtomicUsize::new(0),
        done: Semaphore::new(0),
        expect_close,
        deadline: crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::Construction)?
            .checked_add(30_000_000_000)
            .ok_or(Error::Construction)?,
    })
    .map_err(|_| Error::Construction)?;
    let mut sender_closed = false;
    let result = exercise_readers(&state, count, spread, &mut sender_closed);
    if result.is_err() {
        crate::pr_info!(
            "signal contention failed: readers={} spread={} attempts={} delivered={} failed={} waiters={}\n",
            count,
            spread,
            state.attempts.load(Ordering::Acquire),
            state.delivered.load(Ordering::Acquire),
            state.failed.load(Ordering::Acquire),
            state.receiver.signal_waiter_count_for_test()
        );
    }
    // Also release any remaining waiters on a failed assertion. Each worker owns
    // its state independently, so early failure cannot invalidate its argument.
    if !sender_closed {
        state.sender.close_for_test();
    }
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    result
}

fn exercise_readers(
    state: &FallibleArc<Readers>,
    count: usize,
    spread: bool,
    sender_closed: &mut bool,
) -> Result<(), Error> {
    for index in 0..count {
        let cpu = if spread {
            index % crate::cpu::online_cpu_count()
        } else {
            0
        };
        let cpu = CpuIndex::new(cpu).ok_or(Error::Construction)?;
        let pointer = Box::into_raw(try_box(state.clone()).map_err(|_| Error::Construction)?);
        let thread = match scheduler::kthread_create_with_affinity(
            "channel/contention",
            reader,
            pointer.expose_provenance(),
            CpuMask::single(cpu),
        ) {
            Ok(thread) => thread,
            Err(error) => {
                // SAFETY: failed creation never published the worker argument.
                drop(unsafe { Box::from_raw(pointer) });
                return Err(Error::Scheduler(error));
            }
        };
        scheduler::thread_ready(thread).map_err(Error::Scheduler)?;
    }
    wait_progress(|| state.receiver.signal_waiter_count_for_test() == count)?;
    if state.expect_close {
        // Peer close must release every blocked observer, across batch boundaries.
        state.sender.close_for_test();
        *sender_closed = true;
        wait_progress(|| state.delivered.load(Ordering::Acquire) == count)?;
        for _ in 0..count {
            state.done.acquire().map_err(|_| Error::Progress)?;
        }
        return if state.failed.load(Ordering::Acquire) == 0 {
            Ok(())
        } else {
            Err(Error::Progress)
        };
    }
    let mut expected_attempts = 0;
    for delivered in 0..count {
        let remaining = count - delivered;
        let message =
            PreparedByteMessage::try_copy_from(&state.domain, b"x").map_err(Error::Channel)?;
        state
            .sender
            .prepare_write(&message)
            .map_err(Error::Channel)?
            .publish(message);
        expected_attempts += remaining;
        wait_progress(|| {
            state.delivered.load(Ordering::Acquire) == delivered + 1
                && state.attempts.load(Ordering::Acquire) >= expected_attempts
                && state.receiver.signal_waiter_count_for_test() == remaining - 1
        })?;
    }
    for _ in 0..count {
        state.done.acquire().map_err(|_| Error::Progress)?;
    }
    if state.failed.load(Ordering::Acquire) != 0 {
        return Err(Error::Progress);
    }
    let metrics = state.receiver.signal_metrics_for_test();
    // Registration can win before park. Such a successful observation does not
    // enqueue Ready, so ready counts are measurements, not a scheduling oracle.
    crate::pr_info!(
        "signal contention: readers={} spread={} delivered={} retries={} scans={} scheduler_entries={} cpu_locks={} ready={} critical_ticks={}\n",
        count,
        spread,
        count,
        state.attempts.load(Ordering::Acquire) - count,
        metrics.scans,
        metrics.scheduler_entries,
        metrics.cpu_locks,
        metrics.made_ready,
        metrics.critical_ticks
    );
    Ok(())
}

fn wait_progress(predicate: impl FnMut() -> bool) -> Result<(), Error> {
    let mut predicate = predicate;
    if crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(predicate())
    })
    .map_err(|_| Error::Progress)?
    {
        Ok(())
    } else {
        Err(Error::Progress)
    }
}

extern "C" fn reader(argument: usize) {
    // SAFETY: this one-shot worker receives the Box from exercise_readers.
    let state = unsafe {
        Box::from_raw(core::ptr::with_exposed_provenance_mut::<FallibleArc<Readers>>(argument))
    };
    if run_reader(&state).is_err() {
        state.failed.fetch_add(1, Ordering::Release);
    }
    let _ = state.done.release();
}

fn run_reader(state: &Readers) -> Result<(), ()> {
    let source = state.receiver.signal_source().ok_or(())?;
    loop {
        let outcome = crate::object::wait_one(
            source,
            &state.domain,
            ByteChannel::READABLE.union(ByteChannel::PEER_CLOSED).bits(),
            state.deadline,
            || false,
        )
        .map_err(|_| ())?;
        let SignalWaitOutcome::Observed(snapshot) = outcome else {
            return Err(());
        };
        if snapshot.signals().intersects(ByteChannel::PEER_CLOSED) {
            return if state.expect_close {
                state.delivered.fetch_add(1, Ordering::Release);
                Ok(())
            } else {
                Err(())
            };
        }
        state.attempts.fetch_add(1, Ordering::Release);
        let info = match state.receiver.peek() {
            Ok(info) => info,
            Err(ByteChannelError::WouldBlock | ByteChannelError::Busy) => continue,
            Err(_) => return Err(()),
        };
        let claim = match state.receiver.claim(info) {
            Ok(claim) => claim,
            Err(ByteChannelError::Busy | ByteChannelError::StaleMessage) => continue,
            Err(_) => return Err(()),
        };
        let valid = claim.bytes() == b"x";
        claim.commit().release();
        if !valid {
            return Err(());
        }
        state.delivered.fetch_add(1, Ordering::Release);
        return Ok(());
    }
}
