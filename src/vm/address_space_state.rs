// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure destruction policy for VM-owned translation state.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IdentifierState {
    Reserved,
    Active,
    Retired,
    UnpublishedFailure,
}

pub(super) const fn destruction_is_safe(state: IdentifierState) -> bool {
    match state {
        IdentifierState::Reserved | IdentifierState::UnpublishedFailure => true,
        IdentifierState::Active => false,
        IdentifierState::Retired => true,
    }
}

/// Only a live unpublished reservation may enter the consuming activation
/// transaction. In particular, Active must be rejected without replacement.
pub(super) const fn activation_may_begin(state: IdentifierState) -> bool {
    matches!(state, IdentifierState::Reserved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_never_published_identifiers_may_be_destroyed() {
        assert!(destruction_is_safe(IdentifierState::Reserved));
        assert!(destruction_is_safe(IdentifierState::UnpublishedFailure));
        assert!(destruction_is_safe(IdentifierState::Retired));
        assert!(!destruction_is_safe(IdentifierState::Active));
        assert!(activation_may_begin(IdentifierState::Reserved));
        assert!(!activation_may_begin(IdentifierState::Active));
        assert!(!activation_may_begin(IdentifierState::UnpublishedFailure));
        assert!(!activation_may_begin(IdentifierState::Retired));
    }
}
