// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-lock-owned outgoing context handoff, independent of queue residence.
//!
//! An outgoing thread may already be queued while the architecture still saves
//! its context. Only the matching incoming switch tail releases that ownership.
//! Callers retain the CPU lock across validation, schedule-state checks and
//! completion; this value introduces neither a lock nor cross-CPU authority.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SwitchingContext<T> {
    pub thread: T,
    pub generation: u64,
    pub disposition: SwitchDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SwitchDisposition {
    Local,
    Coordinated,
}

pub(super) struct SwitchHandoff<T> {
    outgoing: Option<SwitchingContext<T>>,
    next_generation: u64,
    completed_preparations: u64,
}

impl<T: Copy> SwitchHandoff<T> {
    pub const fn new() -> Self {
        Self {
            outgoing: None,
            next_generation: 1,
            completed_preparations: 0,
        }
    }

    pub const fn current(&self) -> Option<SwitchingContext<T>> {
        self.outgoing
    }

    /// Must be called at the existing committed switch preparation point.
    /// Failure is an invariant violation there, never a rollback request.
    /// Generation exhaustion deliberately fails before publishing the handoff.
    pub fn begin(&mut self, thread: T, disposition: SwitchDisposition) -> Option<u64> {
        if self.outgoing.is_some() {
            return None;
        }
        let generation = self.next_generation;
        let next = generation.checked_add(1)?;
        self.next_generation = next;
        self.outgoing = Some(SwitchingContext {
            thread,
            generation,
            disposition,
        });
        self.completed_preparations = self.completed_preparations.saturating_add(1);
        Some(generation)
    }

    pub fn for_ticket(&self, ticket: u64) -> Option<SwitchingContext<T>> {
        self.outgoing
            .filter(|outgoing| outgoing.generation == ticket)
    }

    /// A stale or already consumed generation cannot release ownership.
    /// Tickets are CPU-local; the caller must select the owning CPU domain.
    pub fn complete(&mut self, ticket: u64) -> Option<SwitchingContext<T>> {
        let outgoing = self.for_ticket(ticket)?;
        self.outgoing = None;
        Some(outgoing)
    }

    pub const fn count(&self) -> u64 {
        self.completed_preparations
    }
}

#[cfg(test)]
#[path = "../../../tests/kernel/switch_handoff.rs"]
mod tests;
