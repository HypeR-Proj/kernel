// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Monotonic successfully published participant identities.

const TOKEN_MAX: u64 = hyper::abi::native::HYPER_NATIVE_SYNC_OWNER_TOKEN_MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OwnerState {
    Invalid,
    Live,
    Dead,
}

pub(super) struct Tokens {
    issued: u64,
}

impl Tokens {
    pub(super) const fn new() -> Self {
        Self { issued: 0 }
    }

    /// Call only after all fallible admission and storage construction, while
    /// holding the active-index publication lock. Insertion after this call
    /// must be infallible; failed preparation never consumes a token.
    pub(super) fn publish(&mut self) -> Option<u64> {
        let token = self
            .issued
            .checked_add(1)
            .filter(|token| *token <= TOKEN_MAX)?;
        self.issued = token;
        Some(token)
    }

    pub(super) fn observe(&self, token: u64, active: impl FnOnce() -> bool) -> OwnerState {
        if token == 0 || token > self.issued {
            return OwnerState::Invalid;
        }
        if active() {
            OwnerState::Live
        } else {
            OwnerState::Dead
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dead_identity_is_not_reissued_when_active_storage_is_reused() {
        let mut tokens = Tokens::new();
        let first = tokens.publish();
        assert_eq!(first, Some(1));
        assert_eq!(tokens.observe(1, || true), OwnerState::Live);
        assert_eq!(tokens.observe(1, || false), OwnerState::Dead);
        assert_eq!(tokens.publish(), Some(2));
        assert_eq!(tokens.observe(1, || false), OwnerState::Dead);
        assert_eq!(tokens.observe(2, || true), OwnerState::Live);
    }
    #[test]
    fn unknown_and_exhausted_tokens_never_become_dead_by_wrapping() {
        let mut tokens = Tokens::new();
        assert_eq!(tokens.observe(0, || true), OwnerState::Invalid);
        assert_eq!(tokens.observe(1, || true), OwnerState::Invalid);
        tokens.issued = TOKEN_MAX - 1;
        assert_eq!(tokens.publish(), Some(TOKEN_MAX));
        assert_eq!(tokens.publish(), None);
        assert_eq!(tokens.observe(TOKEN_MAX + 1, || false), OwnerState::Invalid);
        assert_eq!(tokens.observe(TOKEN_MAX, || false), OwnerState::Dead);
    }
}
