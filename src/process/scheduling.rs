// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable initial scheduling and authority granted at process construction.

use super::Process;
use crate::authority::Rights;
use crate::task::policy::SchedulingParameters;
use {
    crate::task::scheduling_authority::SCHEDULING_RIGHTS,
    crate::task::scheduling_authority::SchedulingLimits,
};

#[derive(Clone, Copy)]
pub(crate) struct ProcessScheduling {
    pub(crate) initial: SchedulingParameters,
    pub(crate) limits: SchedulingLimits,
    pub(crate) delegated: Rights,
}
impl ProcessScheduling {
    pub(crate) fn default_native() -> Self {
        let initial =
            crate::task::scheduler::default_parameters(crate::task::policy::SchedulingPolicy::Fair);
        Self {
            initial,
            limits: SchedulingLimits::inherited(initial),
            delegated: Rights::SCHED_LOWER,
        }
    }
    /// Process creation is an App boundary unless a supervisor explicitly grants
    /// another class. A demoted ordinary creator cannot regain its former weight
    /// by starting a fresh process. In-process threads instead inherit exactly.
    pub(crate) fn child_initial(current: SchedulingParameters) -> SchedulingParameters {
        use {crate::task::policy::SchedulingPolicy, crate::task::policy::SchedulingRole};
        let base = Self::default_native().initial;
        if current.policy() != SchedulingPolicy::Fair || current.role() == SchedulingRole::Priority
        {
            return base;
        }
        match SchedulingParameters::fair(
            current.weight().min(base.weight()),
            base.quantum_ns(),
            current.band().max(base.band()),
            base.role(),
            base.fair_backend(),
        ) {
            Some(parameters) => parameters,
            None => hyper::debug::invariant_failure("native child scheduling defaults"),
        }
    }
}
impl Process {
    pub(crate) fn scheduling(&self) -> ProcessScheduling {
        self.inner
            .scheduling
            .get()
            .copied()
            .unwrap_or_else(ProcessScheduling::default_native)
    }

    /// Called only before startup capabilities and the initial Thread are built.
    pub(crate) fn set_initial_scheduling(&self, grant: ProcessScheduling) {
        if !SCHEDULING_RIGHTS.contains(grant.delegated)
            || !grant.limits.permits(grant.initial)
            || self.inner.scheduling.publish(grant).is_err()
        {
            hyper::debug::invariant_failure("process scheduling publication invariant");
        }
    }
}
