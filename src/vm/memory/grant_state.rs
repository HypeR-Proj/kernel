// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One-shot device admission. The owning address-space lock serializes every
//! transition, including Native cancellation and guest admission.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum State {
    Created,
    Admitted { route: u64 },
    Quiescent,
    Retired,
}

impl State {
    pub(crate) fn admit(&mut self, route: u64) -> bool {
        if route == 0 || *self != Self::Created {
            return false;
        }
        *self = Self::Admitted { route };
        true
    }

    pub(crate) fn owns(&self, route: u64) -> bool {
        *self == Self::Admitted { route }
    }

    pub(crate) fn quiesce(&mut self, route: u64) -> bool {
        if !self.owns(route) {
            return false;
        }
        *self = Self::Quiescent;
        true
    }

    /// Retirement removes admission before leaf removal and the hardware
    /// acknowledgement barrier. The backing remains owned through that barrier.
    pub(crate) fn retire(&mut self) -> bool {
        if !self.can_retire() {
            return false;
        }
        *self = Self::Retired;
        true
    }

    pub(crate) const fn can_retire(self) -> bool {
        matches!(self, Self::Created | Self::Quiescent)
    }
}

#[cfg(test)]
mod tests {
    use super::State;

    #[test]
    fn guest_proof_is_route_bound_and_tokens_are_one_shot() {
        let mut state = State::Created;
        assert!(state.admit(7));
        assert!(!state.admit(7));
        assert!(!state.admit(8));
        assert!(!state.quiesce(8));
        assert!(!state.retire());
        assert!(state.owns(7));
        assert!(state.quiesce(7));
        assert!(!state.admit(8));
        assert!(!state.quiesce(7));
        assert!(state.retire());
        assert!(!state.admit(7));
    }

    #[test]
    fn cancellation_cannot_race_past_admission() {
        let mut cancelled = State::Created;
        assert!(cancelled.retire());
        assert!(!cancelled.admit(1));
        let mut admitted = State::Created;
        assert!(!admitted.admit(0));
        assert!(admitted.admit(1));
        assert!(!admitted.retire());
    }
}
