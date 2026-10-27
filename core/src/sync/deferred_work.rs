// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lost-wakeup-safe ownership for IRQ-prompted deferred workers.

#[cfg(not(loom))]
use super::atomic::{AtomicU8, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU8, Ordering};

/// Result of transferring a worker toward its blocking wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkDisposition {
    /// The worker retained wake ownership and must perform another batch.
    Continue,
    /// The worker released ownership and may consume its notification.
    Wait,
}

/// Ownership shared by producers, IRQ prompt service, and one worker.
///
/// One atomic state word orders durable work, worker/wake ownership, and the
/// unconsumed IRQ prompt. A producer publishes work before electing a prompt;
/// IRQ service converts that prompt into wake ownership; the active worker
/// retains ownership until it proves no request raced its transition to wait.
pub struct DeferredWork {
    state: AtomicU8,
}

const WORK_PENDING: u8 = 1 << 0;
const WAKE_OUTSTANDING: u8 = 1 << 1;
const IRQ_PROMPTED: u8 = 1 << 2;

impl DeferredWork {
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    // Loom atomics register with the running model and cannot be const.
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    /// Gives a newly readied worker the initial, notification-free ownership.
    pub fn claim_initial_worker(&self) -> bool {
        let mut observed = self.state.load(Ordering::Relaxed);
        loop {
            if observed & WAKE_OUTSTANDING != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed | WAKE_OUTSTANDING,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(current) => observed = current,
            }
        }
    }

    /// Publishes work and reports whether this producer elected an IRQ prompt.
    #[must_use = "the elected producer must issue the deferred-service prompt"]
    pub fn request(&self) -> bool {
        let mut observed = self.state.load(Ordering::Relaxed);
        loop {
            let elected = observed & (WAKE_OUTSTANDING | IRQ_PROMPTED) == 0;
            let next = observed | WORK_PENDING | if elected { IRQ_PROMPTED } else { 0 };
            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return elected,
                Err(current) => observed = current,
            }
        }
    }

    /// Consumes one coalesced producer prompt at a safe IRQ service boundary.
    pub fn consume_prompt(&self) -> bool {
        self.state.fetch_and(!IRQ_PROMPTED, Ordering::AcqRel) & IRQ_PROMPTED != 0
    }

    /// Claims responsibility for issuing one scheduler wake from IRQ service.
    pub fn claim_notification(&self) -> bool {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            if observed & WORK_PENDING == 0 || observed & WAKE_OUTSTANDING != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed | WAKE_OUTSTANDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => observed = current,
            }
        }
    }

    /// Starts one batch and makes later producer requests independently visible.
    pub fn begin_batch(&self) {
        self.state.fetch_and(!WORK_PENDING, Ordering::AcqRel);
    }

    /// Releases active ownership while retaining work and an IRQ prompt.
    pub fn defer_until_irq(&self) {
        let mut observed = self.state.load(Ordering::Relaxed);
        loop {
            let next = (observed | WORK_PENDING | IRQ_PROMPTED) & !WAKE_OUTSTANDING;
            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }

    /// Retains ownership for pending work or safely transfers it toward wait.
    pub fn finish_batch(&self, more_work: bool) -> WorkDisposition {
        if more_work {
            return WorkDisposition::Continue;
        }

        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            if observed & WORK_PENDING != 0 {
                return WorkDisposition::Continue;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed & !WAKE_OUTSTANDING,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return WorkDisposition::Wait,
                Err(current) => observed = current,
            }
        }
    }
}

impl Default for DeferredWork {
    fn default() -> Self {
        Self::new()
    }
}
