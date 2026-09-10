// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exact cancellation and owner retention for movable reserved timer arms.

use crate::kernel::time::ReservedTimer;
use hyper::mm::FallibleArc;
use hyper::sync::atomic::{AtomicUsize, Ordering};

static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug)]
pub(super) enum Error {
    Allocation,
    Timer,
    Progress,
    CallbackCount,
}
fn notify(_: usize) {
    CALLBACKS.fetch_add(1, Ordering::Release);
}

pub(super) fn run() -> Result<(), Error> {
    CALLBACKS.store(0, Ordering::Relaxed);
    let reservation = FallibleArc::try_new(ReservedTimer::try_new().map_err(|_| Error::Timer)?)
        .map_err(|_| Error::Allocation)?;
    // Cancellation returns the same reserved node for the next generation.
    for _ in 0..16 {
        let deadline =
            crate::kernel::time::deadline_after(10_000_000_000).map_err(|_| Error::Timer)?;
        let arm = ReservedTimer::arm_owned(reservation.clone(), deadline, notify, 0)
            .map_err(|_| Error::Timer)?;
        arm.retire().map_err(|_| Error::Timer)?;
    }
    if CALLBACKS.load(Ordering::Acquire) != 0 {
        return Err(Error::CallbackCount);
    }
    for expected in 1..=8 {
        let deadline = crate::kernel::time::deadline_after(100_000).map_err(|_| Error::Timer)?;
        let arm = ReservedTimer::arm_owned(reservation.clone(), deadline, notify, 0)
            .map_err(|_| Error::Timer)?;
        let progress = crate::kernel::task::wait_for_test_progress(
            crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
            || {
                Ok::<_, crate::kernel::task::SleepError>(
                    CALLBACKS.load(Ordering::Acquire) >= expected,
                )
            },
        );
        // Retire even if progress fails: the callback owner never escapes a
        // failed assertion, and a completed callback is joined before rearm.
        arm.retire().map_err(|_| Error::Timer)?;
        if !progress.map_err(|_| Error::Progress)? {
            return Err(Error::Progress);
        }
        if CALLBACKS.load(Ordering::Acquire) != expected {
            return Err(Error::CallbackCount);
        }
    }
    let deadline = crate::kernel::time::deadline_after(10_000_000_000).map_err(|_| Error::Timer)?;
    let arm =
        ReservedTimer::arm_owned(reservation, deadline, notify, 0).map_err(|_| Error::Timer)?;
    // Only the armed token owns the allocation now; it remains stable through cancellation.
    arm.retire().map_err(|_| Error::Timer)?;
    Ok(())
}
