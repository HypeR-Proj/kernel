// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Opens the real worker's retry-check-to-sleep interleaving in kernel tests.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const OFF: u8 = 0;
const ARMED: u8 = 1;
const ENTERED: u8 = 2;
const PUBLISHED: u8 = 3;
const FAILED: u8 = 4;
const LIMIT_NS: u64 = 2_000_000_000;

static STATE: AtomicU8 = AtomicU8::new(OFF);
static RELEASE: AtomicBool = AtomicBool::new(false);

pub(crate) fn arm() -> bool {
    RELEASE.store(false, Ordering::Release);
    STATE
        .compare_exchange(OFF, ARMED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub(crate) fn entered() -> bool {
    STATE.load(Ordering::Acquire) == ENTERED
}

pub(crate) fn published() -> bool {
    STATE.load(Ordering::Acquire) == PUBLISHED
}

pub(crate) fn release() {
    RELEASE.store(true, Ordering::Release);
}

/// Also releases an assertion-error path and prompts any stranded real work.
pub(crate) fn finish() {
    STATE.store(OFF, Ordering::Release);
    RELEASE.store(true, Ordering::Release);
    super::request();
}

pub(super) fn pause_after_retry_check(retry_pending: bool) {
    if !retry_pending
        || super::RETRY_DUE.load(Ordering::Acquire)
        || STATE
            .compare_exchange(ARMED, ENTERED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let Ok(deadline) = crate::time::deadline_after(LIMIT_NS) else {
        STATE.store(FAILED, Ordering::Release);
        return;
    };
    // No timer or retirement lock is held. Sleeping permits the actual timer
    // callback to run even when its worker shares this reaper's physical CPU.
    // The controller releases its address-space owner before opening RELEASE.
    while STATE.load(Ordering::Acquire) == ENTERED {
        if RELEASE.load(Ordering::Acquire) && super::RETRY_DUE.load(Ordering::Acquire) {
            // RETRY_DUE precedes request() inside the callback. Join the real
            // invocation so the durable work bit is certainly published
            // before this pass proceeds toward its empty-to-sleep handoff.
            if super::RETRY_TIMER
                .get()
                .is_none_or(|timer| timer.drain().is_err())
            {
                let _ =
                    STATE.compare_exchange(ENTERED, FAILED, Ordering::AcqRel, Ordering::Acquire);
                return;
            }
            let _ = STATE.compare_exchange(ENTERED, PUBLISHED, Ordering::AcqRel, Ordering::Acquire);
            return;
        }
        if hyper::hal::timer::deadline_reached(crate::time::monotonic_ticks(), deadline)
            || crate::task::sleep_ms(1).is_err()
        {
            let _ = STATE.compare_exchange(ENTERED, FAILED, Ordering::AcqRel, Ordering::Acquire);
            return;
        }
    }
}
