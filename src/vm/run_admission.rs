// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One-way admission for VM execution intervals.
//!
//! The packed word gives close and admission one linearization point. Closing
//! prevents new claims but deliberately does not remove a VM: policy must
//! still stop and join every vCPU, wait for the admitted count to drain, and
//! retire stage-2 translations before registry ownership can be withdrawn.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};

const CLOSED: usize = 1 << (usize::BITS - 1);
const COUNT_MASK: usize = CLOSED - 1;

pub(super) struct RunAdmission {
    owner: u64,
    state: AtomicUsize,
}

impl RunAdmission {
    #[cfg(not(loom))]
    pub(super) const fn new(owner: u64) -> Self {
        Self {
            owner,
            state: AtomicUsize::new(0),
        }
    }

    #[cfg(loom)]
    pub(super) fn new(owner: u64) -> Self {
        Self {
            owner,
            state: AtomicUsize::new(0),
        }
    }

    pub(super) fn admit(&self) -> Result<RunAdmissionClaim, AdmissionError> {
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            if current & CLOSED != 0 {
                return Err(AdmissionError::Closed);
            }
            if current == COUNT_MASK {
                return Err(AdmissionError::CountExhausted);
            }
            match self.state.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(RunAdmissionClaim {
                        owner: self.owner,
                        armed: true,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Permanently closes admission and returns the claims active at its
    /// linearization point. VM quiescence closes this before withdrawing routes.
    pub(super) fn close(&self) -> usize {
        self.state.fetch_or(CLOSED, Ordering::AcqRel) & COUNT_MASK
    }

    /// Reports quiescence after close and acquires every prior claim release.
    pub(super) fn is_closed_and_quiescent(&self) -> bool {
        self.state.load(Ordering::Acquire) == CLOSED
    }

    pub(super) fn release(&self, mut claim: RunAdmissionClaim) {
        if claim.owner != self.owner || !claim.armed {
            fail_stop_invalid_claim()
        }
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            if current & COUNT_MASK == 0 {
                fail_stop_invalid_claim()
            }
            match self.state.compare_exchange_weak(
                current,
                current - 1,
                // Close prevents new admissions. Acquiring the preceding
                // packed-state update makes successive releases a cumulative
                // chain, so a quiescence acquire observes every admitted run,
                // not only whichever claim happened to drain last.
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    claim.armed = false;
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    #[cfg(test)]
    pub(super) fn active_count(&self) -> usize {
        self.state.load(Ordering::Relaxed) & COUNT_MASK
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AdmissionError {
    Closed,
    CountExhausted,
}

#[must_use = "a VM run-admission claim must be released after execution stops"]
pub(super) struct RunAdmissionClaim {
    owner: u64,
    armed: bool,
}

impl Drop for RunAdmissionClaim {
    fn drop(&mut self) {
        if self.armed {
            fail_stop_invalid_claim()
        }
    }
}

#[cold]
fn fail_stop_invalid_claim() -> ! {
    hyper::debug::invariant_failure("invalid or abandoned VM run admission claim")
}
