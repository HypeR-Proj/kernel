// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Source-window transitions, serialized by the owning window's lock. This
//! state never owns payload bytes; accepted leases keep that backing alive.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Busy,
    Invalid,
    ResourceLimit,
}

pub(super) struct State {
    bound: bool,
    filling: bool,
    failed: bool,
    generation: u64,
    active: bool,
    length: usize,
    leases: usize,
}

impl State {
    pub(super) const fn new() -> Self {
        Self {
            bound: false,
            filling: false,
            failed: false,
            generation: 0,
            active: false,
            length: 0,
            leases: 0,
        }
    }

    pub(super) fn bind(&mut self) -> Result<(), Error> {
        if self.bound || self.failed {
            return Err(Error::Busy);
        }
        self.bound = true;
        Ok(())
    }

    /// An unpublished, idle mount may roll back. Any exposure or live fill
    /// permanently retires this backing instead of assigning it to a new mount.
    pub(super) fn release_binding(&mut self, published: bool) {
        if published || self.active || self.filling || self.leases != 0 {
            self.retire();
        } else {
            self.bound = false;
        }
    }

    pub(super) fn begin_fill(&mut self) -> Result<(), Error> {
        if !self.bound || self.failed || self.active || self.filling || self.leases != 0 {
            return Err(Error::Busy);
        }
        self.generation = self.generation.checked_add(1).ok_or(Error::ResourceLimit)?;
        self.filling = true;
        Ok(())
    }

    /// Source bytes become submit-able only after the boundary copy succeeds.
    /// Retirement while that copy ran cannot be undone by late publication.
    pub(super) fn publish_fill(&mut self, length: usize, success: bool) -> Result<u64, Error> {
        if !self.filling {
            return Err(Error::Invalid);
        }
        self.filling = false;
        if !self.bound || self.failed {
            return Err(Error::Busy);
        }
        if success {
            self.length = length;
            self.active = true;
        }
        Ok(self.generation)
    }

    pub(super) fn acquire_lease(
        &mut self,
        token: u64,
        ranges: &[(usize, usize)],
    ) -> Result<(), Error> {
        if !self.active
            || self.failed
            || token == 0
            || token != self.generation
            || ranges.iter().any(|(offset, length)| {
                offset
                    .checked_add(*length)
                    .is_none_or(|end| end > self.length)
            })
        {
            return Err(Error::Invalid);
        }
        self.leases = self.leases.checked_add(1).ok_or(Error::ResourceLimit)?;
        Ok(())
    }

    pub(super) fn release_lease(&mut self) -> bool {
        let Some(leases) = self.leases.checked_sub(1) else {
            return false;
        };
        self.leases = leases;
        true
    }

    /// Reply invalidates the token before checking accepted requests. A reply
    /// with live leases fails closed even if those requests complete later.
    pub(super) fn finish(&mut self) -> Result<(), Error> {
        self.active = false;
        if self.failed || self.leases != 0 {
            self.failed = true;
            Err(Error::Busy)
        } else {
            Ok(())
        }
    }

    pub(super) fn retire(&mut self) {
        self.active = false;
        self.failed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, State};

    fn token(result: Result<u64, Error>) -> u64 {
        match result {
            Ok(token) => token,
            Err(error) => panic!("unexpected publication error: {error:?}"),
        }
    }

    fn active() -> (State, u64) {
        let mut state = State::new();
        assert_eq!(state.bind(), Ok(()));
        assert_eq!(state.begin_fill(), Ok(()));
        let token = token(state.publish_fill(1024, true));
        (state, token)
    }

    #[test]
    fn stale_token_cannot_submit_after_reply_or_into_next_write() {
        let (mut state, old) = active();
        assert_eq!(state.finish(), Ok(()));
        assert_eq!(state.acquire_lease(old, &[(0, 512)]), Err(Error::Invalid));
        assert_eq!(state.begin_fill(), Ok(()));
        let next = token(state.publish_fill(512, true));
        assert_ne!(old, next);
        assert_eq!(state.acquire_lease(old, &[(0, 512)]), Err(Error::Invalid));
        assert_eq!(state.acquire_lease(next, &[(0, 512)]), Ok(()));
        assert!(state.release_lease());
        assert_eq!(state.finish(), Ok(()));
    }

    #[test]
    fn early_reply_retires_window_even_after_last_completion() {
        let (mut state, token) = active();
        assert_eq!(state.acquire_lease(token, &[(0, 512)]), Ok(()));
        assert_eq!(state.acquire_lease(token, &[(512, 512)]), Ok(()));
        assert!(state.release_lease());
        assert_eq!(state.finish(), Err(Error::Busy));
        assert_eq!(state.acquire_lease(token, &[(0, 512)]), Err(Error::Invalid));
        assert_eq!(state.begin_fill(), Err(Error::Busy));
        assert!(state.release_lease());
        assert_eq!(state.begin_fill(), Err(Error::Busy));
        state.release_binding(false);
        assert_eq!(state.bind(), Err(Error::Busy));
    }

    #[test]
    fn completion_before_reply_permits_reuse_but_not_before_reply() {
        let (mut state, token) = active();
        assert_eq!(state.acquire_lease(token, &[(0, 512)]), Ok(()));
        assert!(state.release_lease());
        assert_eq!(state.begin_fill(), Err(Error::Busy));
        assert_eq!(state.finish(), Ok(()));
        assert_eq!(state.begin_fill(), Ok(()));
    }

    #[test]
    fn only_unpublished_idle_binding_can_roll_back() {
        let mut state = State::new();
        assert_eq!(state.begin_fill(), Err(Error::Busy));
        assert_eq!(state.bind(), Ok(()));
        assert_eq!(state.bind(), Err(Error::Busy));
        state.release_binding(false);
        assert_eq!(state.bind(), Ok(()));
        state.release_binding(true);
        assert_eq!(state.bind(), Err(Error::Busy));
        assert_eq!(state.begin_fill(), Err(Error::Busy));
    }

    #[test]
    fn retirement_during_copy_cannot_publish_late_source() {
        let mut state = State::new();
        assert_eq!(state.bind(), Ok(()));
        assert_eq!(state.begin_fill(), Ok(()));
        state.release_binding(false);
        assert_eq!(state.publish_fill(1024, true), Err(Error::Busy));
        assert_eq!(state.acquire_lease(1, &[(0, 512)]), Err(Error::Invalid));
        assert_eq!(state.bind(), Err(Error::Busy));
    }

    #[test]
    fn failed_copy_never_authorizes_partial_bytes() {
        let mut state = State::new();
        assert_eq!(state.bind(), Ok(()));
        assert_eq!(state.begin_fill(), Ok(()));
        let failed = token(state.publish_fill(1024, false));
        assert_eq!(
            state.acquire_lease(failed, &[(0, 512)]),
            Err(Error::Invalid)
        );
        assert_eq!(state.begin_fill(), Ok(()));
        let next = token(state.publish_fill(512, true));
        assert_ne!(failed, next);
        assert_eq!(
            state.acquire_lease(failed, &[(0, 512)]),
            Err(Error::Invalid)
        );
        assert_eq!(state.acquire_lease(next, &[(0, 512)]), Ok(()));
    }

    #[test]
    fn aborted_active_write_cannot_be_reopened() {
        let (mut state, token) = active();
        state.retire();
        assert_eq!(state.acquire_lease(token, &[(0, 512)]), Err(Error::Invalid));
        assert_eq!(state.finish(), Err(Error::Busy));
        assert_eq!(state.begin_fill(), Err(Error::Busy));
    }

    #[test]
    fn rejected_ranges_and_tokens_do_not_retain_leases() {
        let (mut state, token) = active();
        for (candidate, ranges) in [
            (0, [(0, 512)]),
            (token + 1, [(0, 512)]),
            (token, [(513, 512)]),
            (token, [(usize::MAX, 512)]),
        ] {
            assert_eq!(state.acquire_lease(candidate, &ranges), Err(Error::Invalid));
        }
        assert!(!state.release_lease());
        assert_eq!(state.finish(), Ok(()));
    }

    #[test]
    fn generation_and_lease_count_cannot_wrap() {
        let (mut state, token) = active();
        state.leases = usize::MAX;
        assert_eq!(
            state.acquire_lease(token, &[(0, 512)]),
            Err(Error::ResourceLimit)
        );
        assert_eq!(state.leases, usize::MAX);
        state.leases = 0;
        assert_eq!(state.finish(), Ok(()));
        state.generation = u64::MAX;
        assert_eq!(state.begin_fill(), Err(Error::ResourceLimit));
        assert_eq!(state.publish_fill(512, true), Err(Error::Invalid));
        assert_eq!(state.acquire_lease(token, &[(0, 512)]), Err(Error::Invalid));
    }
}
