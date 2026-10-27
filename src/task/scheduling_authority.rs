// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduling grants are checked against the current state at the scheduler's
//! commit point. Ordinary object WRITE authority cannot elevate scheduling.

use super::policy::{
    SchedulingParameters, SchedulingPolicy, SchedulingRole, ThreadPriority, ThreadWeight,
};
use crate::authority::Rights;

pub(crate) const SCHEDULING_RIGHTS: Rights = Rights::SCHED_LOWER
    .union(Rights::SCHED_PRIORITY)
    .union(Rights::SCHED_CLASS)
    .union(Rights::SCHED_BUDGET);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulingAuthority {
    rights: Rights,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulingPermissionError {
    AccessDenied,
    OutsideLimits,
}

/// Immutable object ceiling. Handle attenuation can remove operations but cannot
/// enlarge the configurations admitted when the target was created.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulingLimits {
    policy_mask: u8,
    role_mask: u8,
    highest_band: ThreadPriority,
    highest_priority: ThreadPriority,
    maximum_weight: u16,
    maximum_budget_ns: u64,
}

impl SchedulingLimits {
    pub(crate) const SYSTEM: Self = Self {
        policy_mask: 7,
        role_mask: 7,
        highest_band: ThreadPriority::HIGHEST,
        highest_priority: ThreadPriority::HIGHEST,
        maximum_weight: ThreadWeight::MAX,
        maximum_budget_ns: 1_000_000_000,
    };

    /// Children with no elevation grant can retain or reduce inherited treatment.
    pub(crate) fn inherited(parameters: SchedulingParameters) -> Self {
        Self {
            policy_mask: policy_bit(parameters.policy()),
            role_mask: role_bit(parameters.role()),
            highest_band: parameters.band(),
            highest_priority: parameters
                .policy()
                .priority()
                .unwrap_or(ThreadPriority::LOWEST),
            maximum_weight: parameters.weight().get(),
            maximum_budget_ns: parameters.budget_parameters().map_or(0, |p| p.budget_ns()),
        }
    }

    pub(crate) fn permits(self, parameters: SchedulingParameters) -> bool {
        self.policy_mask & policy_bit(parameters.policy()) != 0
            && self.role_mask & role_bit(parameters.role()) != 0
            && parameters.band() >= self.highest_band
            && parameters
                .policy()
                .priority()
                .is_none_or(|p| p >= self.highest_priority)
            && parameters.weight().get() <= self.maximum_weight
            && parameters
                .budget_parameters()
                .is_none_or(|p| p.budget_ns() <= self.maximum_budget_ns)
    }
}

impl SchedulingAuthority {
    pub(crate) const fn new(rights: Rights) -> Self {
        Self {
            rights: rights.intersection(SCHEDULING_RIGHTS),
        }
    }

    /// A read-only builder handle must not mutate child grants by submitting
    /// unchanged parameters. Explicit scheduling authority may attenuate grants
    /// and delegate only a subset of the rights it already holds.
    pub(crate) fn validate_delegation(
        self,
        old: Rights,
        requested: Rights,
    ) -> Result<(), SchedulingPermissionError> {
        if !self.rights.contains(requested)
            || (old != requested.union(Rights::SCHED_LOWER) && self.rights.bits() == 0)
        {
            return Err(SchedulingPermissionError::AccessDenied);
        }
        Ok(())
    }

    /// Requires every independent operation implied by the complete new state.
    /// Invoke while the target's old parameters cannot change concurrently.
    pub(crate) fn validate_change(
        self,
        old: SchedulingParameters,
        new: SchedulingParameters,
        limits: SchedulingLimits,
    ) -> Result<(), SchedulingPermissionError> {
        if !limits.permits(new) {
            return Err(SchedulingPermissionError::OutsideLimits);
        }
        if old == new {
            return Ok(());
        }
        let policy_changed = old.policy().class() != new.policy().class();
        if policy_changed || old.role() != new.role() || old.fair_backend() != new.fair_backend() {
            self.require(Rights::SCHED_CLASS)?;
        }
        if old.budget_parameters() != new.budget_parameters() {
            self.require(Rights::SCHED_BUDGET)?;
        }
        let priority_raised = new.band() < old.band()
            || new.weight() > old.weight()
            || match (old.policy().priority(), new.policy().priority()) {
                (Some(old), Some(new)) => new < old,
                (None, Some(_)) => true,
                _ => false,
            };
        let quantum_changed = old.quantum_ns() != new.quantum_ns() && !policy_changed;
        if priority_raised || quantum_changed {
            self.require(Rights::SCHED_PRIORITY)?;
        } else if (old.band() != new.band()
            || old.weight() != new.weight()
            || old.policy().priority() != new.policy().priority())
            && !self.rights.contains(Rights::SCHED_PRIORITY)
        {
            self.require(Rights::SCHED_LOWER)?;
        }
        Ok(())
    }

    fn require(self, right: Rights) -> Result<(), SchedulingPermissionError> {
        if self.rights.contains(right) {
            Ok(())
        } else {
            Err(SchedulingPermissionError::AccessDenied)
        }
    }
}

const fn policy_bit(policy: SchedulingPolicy) -> u8 {
    match policy {
        SchedulingPolicy::Fair => 1,
        SchedulingPolicy::Budget => 2,
        SchedulingPolicy::Fifo { .. } => 4,
        SchedulingPolicy::Idle => 0,
    }
}
const fn role_bit(role: SchedulingRole) -> u8 {
    match role {
        SchedulingRole::Ordinary => 1,
        SchedulingRole::Priority => 2,
        SchedulingRole::Protected => 4,
    }
}
