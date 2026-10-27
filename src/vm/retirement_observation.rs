// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded diagnostics carried by the existing VM retirement owner.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RetirementError {
    DeviceQuarantined,
    TopologyUnavailable,
    TransportBusy,
    Unsupported,
}

impl RetirementError {
    const fn bit(self) -> u8 {
        match self {
            Self::DeviceQuarantined => 1,
            Self::TopologyUnavailable => 2,
            Self::TransportBusy => 4,
            Self::Unsupported => 8,
        }
    }
}

pub(super) struct RetirementObservation {
    observed: u8,
    failures: u32,
}

impl RetirementObservation {
    pub(super) const fn new() -> Self {
        Self {
            observed: 0,
            failures: 0,
        }
    }

    /// Report each cause once per VM incarnation, even if causes alternate.
    /// No allocation, clock dependency or retry-policy decision belongs here.
    pub(super) fn record(&mut self, error: RetirementError) -> bool {
        self.failures = self.failures.saturating_add(1);
        let bit = error.bit();
        let first = self.observed & bit == 0;
        self.observed |= bit;
        first
    }

    pub(super) const fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::{RetirementError, RetirementObservation};

    #[test]
    fn alternating_failures_remain_bounded_and_counted() {
        let mut observation = RetirementObservation::new();
        assert_eq!(observation.failures(), 0);
        let causes = [
            RetirementError::TransportBusy,
            RetirementError::DeviceQuarantined,
            RetirementError::TopologyUnavailable,
            RetirementError::Unsupported,
        ];
        for round in 0..100 {
            for cause in causes {
                assert_eq!(observation.record(cause), round == 0);
            }
        }
        assert_eq!(observation.failures(), 400);
        assert!(RetirementObservation::new().record(RetirementError::TransportBusy));
    }

    #[test]
    fn failure_counter_saturates_without_reopening_reports() {
        let mut observation = RetirementObservation::new();
        assert!(observation.record(RetirementError::DeviceQuarantined));
        observation.failures = u32::MAX;
        assert!(!observation.record(RetirementError::DeviceQuarantined));
        assert_eq!(observation.failures(), u32::MAX);
    }
}
