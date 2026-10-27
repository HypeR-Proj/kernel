// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Thread-owned resources for one blocking wait.
//!
//! Start here when tracing wait lifetime: `wait::WaitRecord` arbitrates the
//! outcome under the owning CPU's scheduler lock; this stable context retains
//! the atomic condition node and tracks signal/timer registrations until their
//! owners finish cleanup. An idle arbitration record alone is not reusable.
//! Signal nodes remain source-owned (one wait may subscribe to many sources),
//! and timers retain their existing exact-handle cancel/join protocol.

use hyper::sync::InterruptSpinLock;

use super::{WaitTicket, scheduler};
use crate::process::atomic_wait::ThreadWaiter;

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

/// Stable across CPU migration. Never hold either member lock while entering
/// the scheduler or another condition source. Registry readers retain Thread
/// lifetime; scheduler admission may inspect this context under its CPU lock.
pub(crate) struct ThreadWaitContext {
    pub(crate) atomic: ThreadWaiter,
    sources: Lock<Sources>,
}

struct Sources {
    ticket: Option<WaitTicket>,
    signals: usize,
    timers: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum WaitSource {
    Signal,
    Timer,
}

impl ThreadWaitContext {
    pub(crate) const fn new() -> Self {
        Self {
            atomic: ThreadWaiter::new(),
            sources: Lock::new(Sources {
                ticket: None,
                signals: 0,
                timers: 0,
            }),
        }
    }

    /// All external registrations must retire before rearming or Thread exit.
    pub(crate) fn is_idle(&self) -> bool {
        self.atomic.is_idle() && self.sources.with(|sources| sources.ticket.is_none())
    }

    fn retain(&self, ticket: WaitTicket, source: WaitSource) {
        self.sources.with(|sources| {
            if sources.ticket.is_some_and(|active| active != ticket) {
                invariant();
            }
            let count = sources.count(source);
            *count = match count.checked_add(1) {
                Some(count) => count,
                None => invariant(),
            };
            sources.ticket = Some(ticket);
        });
    }

    fn release(&self, ticket: WaitTicket, source: WaitSource) {
        self.sources.with(|sources| {
            if sources.ticket != Some(ticket) {
                invariant();
            }
            let count = sources.count(source);
            *count = match count.checked_sub(1) {
                Some(count) => count,
                None => invariant(),
            };
            if sources.signals == 0 && sources.timers == 0 {
                sources.ticket = None;
            }
        });
    }
}

impl Sources {
    fn count(&mut self, source: WaitSource) -> &mut usize {
        match source {
            WaitSource::Signal => &mut self.signals,
            WaitSource::Timer => &mut self.timers,
        }
    }
}

impl Drop for ThreadWaitContext {
    fn drop(&mut self) {
        if !self.is_idle() {
            invariant();
        }
    }
}

/// Cleanup obligation owned by a signal node or timeout context, not a pointer
/// to a caller's stack. Create before publication; drop only after unlink/join,
/// outside source and scheduler locks. It does not itself cancel the source.
pub(crate) struct WaitSourceRegistration {
    ticket: WaitTicket,
    source: WaitSource,
}

impl WaitSourceRegistration {
    pub(crate) fn new(ticket: WaitTicket, source: WaitSource) -> Self {
        if scheduler::with_wait_context(ticket.thread(), |context| context.retain(ticket, source))
            .is_err()
        {
            invariant();
        }
        Self { ticket, source }
    }
}

impl Drop for WaitSourceRegistration {
    fn drop(&mut self) {
        if scheduler::with_wait_context(self.ticket.thread(), |context| {
            context.release(self.ticket, self.source)
        })
        .is_err()
        {
            invariant();
        }
    }
}

fn invariant() -> ! {
    hyper::debug::invariant_failure("Thread wait source lifetime invariant")
}
