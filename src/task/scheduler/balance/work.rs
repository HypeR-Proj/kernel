// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Event publication and pass ownership share one atomic modification order.

#[cfg(not(loom))]
use hyper::sync::atomic::{AtomicU8, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU8, Ordering};

const DIRTY: u8 = 1;
const ACTIVE: u8 = 2;
const CONTINUE: u8 = 4;

pub(super) struct Work {
    state: AtomicU8,
}

impl Work {
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    /// Elects a prompt only if no active owner or prior event covers this work.
    pub(super) fn request(&self) -> bool {
        self.state.fetch_or(DIRTY, Ordering::AcqRel) & (DIRTY | ACTIVE) == 0
    }

    pub(super) fn pending(&self) -> bool {
        self.state.load(Ordering::Acquire) & (DIRTY | CONTINUE) != 0
    }

    /// New events during a sweep remain dirty until its frozen extent completes.
    pub(super) fn begin(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & ACTIVE != 0 || state & (DIRTY | CONTINUE) == 0 {
                return false;
            }
            let next = (state | ACTIVE | CONTINUE)
                & if state & CONTINUE == 0 {
                    !DIRTY
                } else {
                    u8::MAX
                };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(observed) => state = observed,
            }
        }
    }

    /// Relinquishes ownership and elects a prompt for unfinished or newer work.
    /// A request racing this CAS either appears in `state` or sees ACTIVE clear
    /// and elects its own prompt. Neither side can miss both publications.
    pub(super) fn complete(&self, more: bool) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            debug_assert_ne!(state & ACTIVE, 0);
            let next = (state & !(ACTIVE | CONTINUE)) | if more { CONTINUE } else { 0 };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return more || state & DIRTY != 0,
                Err(observed) => state = observed,
            }
        }
    }

    /// A failed lock subscription owns the next prompt. Preserve both the
    /// unfinished sweep and new events, but do not poll that still-held lock.
    pub(super) fn blocked(&self) {
        self.state.fetch_and(!ACTIVE, Ordering::Release);
    }
}
