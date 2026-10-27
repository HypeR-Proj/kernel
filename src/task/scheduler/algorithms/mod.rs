// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free algorithm state, independent of Thread and queue ownership.
//!
//! Callers serialize state under the owning CPU's scheduler lock. A machine
//! switch, interrupt, or migration never implicitly opens a new scheduling turn.

use super::policy::{BudgetParameters, FairBackend, SchedulingParameters, SchedulingPolicy};

pub(crate) mod arbitration;
mod budget;
mod fair;
pub(crate) mod ready_index;

pub(crate) use budget::BudgetRuntime;
pub(crate) use fair::FairRuntime;

/// State travels with its Thread; CPU arbitration windows deliberately do not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PolicyRuntime {
    Fifo,
    Fair(FairRuntime),
    Budget(BudgetRuntime),
    Idle,
}

impl PolicyRuntime {
    pub(crate) fn new(parameters: SchedulingParameters) -> Self {
        match parameters.policy() {
            SchedulingPolicy::Fifo { .. } => Self::Fifo,
            SchedulingPolicy::Fair => Self::Fair(FairRuntime::new(parameters.quantum_ns())),
            SchedulingPolicy::Budget => match parameters.budget_parameters() {
                Some(budget) => Self::Budget(BudgetRuntime::new(budget)),
                None => hyper::debug::invariant_failure("validated Budget policy lacks parameters"),
            },
            SchedulingPolicy::Idle => Self::Idle,
        }
    }

    /// Closes one actual execution interval. `false` rejects reversed time or
    /// a policy/state mismatch; no caller should silently drop that failure.
    pub(crate) fn account(
        &mut self,
        parameters: SchedulingParameters,
        start: u64,
        end: u64,
    ) -> bool {
        let Some(elapsed) = end.checked_sub(start) else {
            return false;
        };
        match (self, parameters.policy()) {
            (Self::Fifo, SchedulingPolicy::Fifo { .. }) | (Self::Idle, SchedulingPolicy::Idle) => {
                true
            }
            (Self::Fair(runtime), SchedulingPolicy::Fair) => {
                runtime.account(elapsed, parameters.weight());
                true
            }
            (Self::Budget(runtime), SchedulingPolicy::Budget) => {
                match parameters.budget_parameters() {
                    Some(budget) => runtime.account_interval(start, end, budget),
                    None => false,
                }
            }
            _ => false,
        }
    }

    pub(crate) fn epoch_refresh(&mut self, now: u64, parameters: SchedulingParameters) {
        if let (Self::Budget(runtime), Some(budget)) = (self, parameters.budget_parameters()) {
            runtime.refresh(now, budget);
        }
    }

    /// Selectors use `turn` to break ties and for the replaceable RR backend.
    pub(crate) fn candidate_key(&self, parameters: SchedulingParameters, turn: u64) -> (u128, u64) {
        match self {
            Self::Fair(runtime) if parameters.fair_backend() == FairBackend::Stride => {
                (runtime.pass(), turn)
            }
            _ => (0, turn),
        }
    }

    /// Returns only algorithm deadlines. The CPU arbiter and timer queue add
    /// their own deadlines. `competition` means a turn can serve another
    /// contender: Fair excludes peers that can only wait for Budget renewal,
    /// while Budget includes peers which may enter idle borrowing.
    pub(crate) fn next_deadline(
        &mut self,
        now: u64,
        parameters: SchedulingParameters,
        competition: bool,
        borrow: bool,
    ) -> Option<u64> {
        if !competition {
            return None;
        }
        match self {
            Self::Fair(runtime) => now.checked_add(runtime.remaining_ns()),
            Self::Budget(runtime) => runtime.deadline(now, parameters.budget_parameters()?, borrow),
            Self::Fifo | Self::Idle => None,
        }
    }

    /// Called only when a real algorithm turn ends, never by an unrelated IRQ
    /// or the start of a new fixed budget period.
    pub(crate) fn finish_turn(&mut self, parameters: SchedulingParameters) {
        match self {
            Self::Fair(runtime) => runtime.finish_turn(parameters.quantum_ns()),
            Self::Budget(runtime) => {
                if let Some(budget) = parameters.budget_parameters() {
                    runtime.finish_turn(budget);
                }
            }
            Self::Fifo | Self::Idle => {}
        }
    }
}
