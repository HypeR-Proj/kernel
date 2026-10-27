// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exact-owner, bounded-rate retries for an early guest timer EOI.

const FIRST_RETRY_NS: u64 = 50_000;
const MAX_RETRY_NS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Update {
    pub(crate) generation: u64,
    pub(crate) delay_ns: Option<u64>,
}

struct Retry {
    owner: u64,
    generation: u64,
    delay_ns: u64,
}

pub(crate) struct Recovery {
    owner: u64,
    generation: u64,
    active: bool,
    retry: Option<Retry>,
}

impl Recovery {
    pub(crate) const fn new() -> Self {
        Self {
            owner: 0,
            generation: 0,
            active: false,
            retry: None,
        }
    }

    /// Every hardware activation/detachment invalidates the old run's work.
    pub(crate) fn change_owner(&mut self, active: bool) -> Option<Update> {
        let owner = self.owner.checked_add(1)?;
        let generation = self.generation.checked_add(1)?;
        self.owner = owner;
        self.generation = generation;
        self.active = active;
        self.retry = None;
        Some(Update {
            generation,
            delay_ns: None,
        })
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.retry.is_some()
    }

    pub(crate) fn accepts(&self, generation: u64) -> bool {
        self.active
            && self
                .retry
                .as_ref()
                .is_some_and(|retry| retry.owner == self.owner && retry.generation == generation)
    }

    /// Only an observed timer EOI while the physical level remains asserted
    /// may start a retry. Inactive sources and unrelated maintenance cannot.
    pub(crate) fn start(&mut self) -> Option<Update> {
        if !self.active || self.retry.is_some() {
            return None;
        }
        self.arm(FIRST_RETRY_NS)
    }

    pub(crate) fn retry(&mut self, generation: u64) -> Option<Update> {
        if !self.accepts(generation) {
            return None;
        }
        let delay_ns = self
            .retry
            .as_ref()?
            .delay_ns
            .saturating_mul(2)
            .min(MAX_RETRY_NS);
        self.arm(delay_ns)
    }

    pub(crate) fn complete(&mut self) -> Option<Update> {
        self.retry.as_ref()?;
        let generation = self.next_generation();
        self.generation = generation;
        self.retry = None;
        Some(Update {
            generation,
            delay_ns: None,
        })
    }

    fn arm(&mut self, delay_ns: u64) -> Option<Update> {
        let generation = self.next_generation();
        self.generation = generation;
        self.retry = Some(Retry {
            owner: self.owner,
            generation,
            delay_ns,
        });
        Some(Update {
            generation,
            delay_ns: Some(delay_ns),
        })
    }

    fn next_generation(&self) -> u64 {
        match self.generation.checked_add(1) {
            Some(generation) => generation,
            None => hyper::debug::invariant_failure("guest timer retry generation exhausted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FIRST_RETRY_NS, MAX_RETRY_NS, Recovery};

    #[test]
    fn inactive_sources_do_not_poll_and_switches_revoke_old_expiries() {
        let mut state = Recovery::new();
        assert!(state.start().is_none());
        assert!(state.change_owner(true).is_some());
        let first = match state.start() {
            Some(update) => update,
            None => panic!("active retry"),
        };
        assert_eq!(first.delay_ns, Some(FIRST_RETRY_NS));
        assert!(state.accepts(first.generation));
        assert!(state.start().is_none());
        assert!(state.change_owner(false).is_some());
        assert!(!state.accepts(first.generation));
        assert!(state.change_owner(true).is_some());
        assert!(state.retry(first.generation).is_none());
        assert!(!state.is_pending());
    }

    #[test]
    fn unresolved_source_backs_off_and_completed_retry_cannot_rearm() {
        let mut state = Recovery::new();
        assert!(state.change_owner(true).is_some());
        let mut update = match state.start() {
            Some(update) => update,
            None => panic!("active retry"),
        };
        for _ in 0..20 {
            update = match state.retry(update.generation) {
                Some(update) => update,
                None => panic!("current retry"),
            };
            assert!(update.delay_ns.is_some_and(|delay| delay <= MAX_RETRY_NS));
        }
        assert_eq!(update.delay_ns, Some(MAX_RETRY_NS));
        assert!(state.complete().is_some());
        assert!(!state.accepts(update.generation));
        assert!(state.retry(update.generation).is_none());
    }
}
