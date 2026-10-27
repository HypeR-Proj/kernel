// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-local protection between replaceable algorithms.
//!
//! The priority allowance and protected floor are shared by all participating
//! Threads on one CPU. Creating or migrating Threads cannot refill either.
//! Protected execution is an aggregate opportunity, not a per-Thread latency
//! promise, and strict FIFO/IRQ execution can delay it.

use super::super::policy::{SchedulingParameters, SchedulingPolicy, SchedulingRole};

/// Normal algorithm ranks retain budget and borrowing gates. Prefer-Fair
/// products use the bounded priority allowance instead of reversing these
/// ranks, which would starve Budget work under permanent Fair demand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArbitrationOrder {
    pub priority_rank: u8,
    pub budget_rank: u8,
    pub fair_rank: u8,
    pub borrow_rank: u8,
}

impl ArbitrationOrder {
    pub(crate) const DEFAULT: Self = Self {
        priority_rank: 0,
        budget_rank: 1,
        fair_rank: 2,
        borrow_rank: 3,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArbitrationConfig {
    period_ns: u64,
    priority_allowance_ns: u64,
    protected_floor_ns: u64,
    order: ArbitrationOrder,
    prefer_fair: bool,
}

impl ArbitrationConfig {
    pub(crate) const fn new(
        period_ns: u64,
        priority_allowance_ns: u64,
        protected_floor_ns: u64,
        order: ArbitrationOrder,
    ) -> Option<Self> {
        if period_ns == 0
            || period_ns > super::BudgetParameters::MAX_PERIOD_NS
            || protected_floor_ns == 0
            || protected_floor_ns >= period_ns
            || priority_allowance_ns >= period_ns - protected_floor_ns
            || order.priority_rank >= order.budget_rank
            || order.budget_rank >= order.fair_rank
            || order.fair_rank >= order.borrow_rank
        {
            None
        } else {
            Some(Self {
                period_ns,
                priority_allowance_ns,
                protected_floor_ns,
                order,
                prefer_fair: false,
            })
        }
    }

    pub(crate) const fn with_prefer_fair(mut self, prefer_fair: bool) -> Self {
        self.prefer_fair = prefer_fair;
        self
    }

    #[cfg(test)]
    pub(crate) const fn period_ns(self) -> u64 {
        self.period_ns
    }
    #[cfg(test)]
    pub(crate) const fn priority_allowance_ns(self) -> u64 {
        self.priority_allowance_ns
    }
    #[cfg(test)]
    pub(crate) const fn protected_floor_ns(self) -> u64 {
        self.protected_floor_ns
    }

    pub(crate) const fn accepts(self, parameters: SchedulingParameters) -> bool {
        match parameters.budget_parameters() {
            Some(budget) => budget.period_ns() == self.period_ns,
            None => true,
        }
    }

    /// Image-wide admission uses one Fair key representation. A queue cannot
    /// compare RR turn numbers with weighted Stride passes from another Thread.
    pub(crate) fn admits(
        self,
        parameters: SchedulingParameters,
        fair_backend: super::FairBackend,
    ) -> bool {
        parameters.policy() != SchedulingPolicy::Idle
            && self.accepts(parameters)
            && (parameters.policy() != SchedulingPolicy::Fair
                || parameters.fair_backend() == fair_backend)
    }
}

/// Includes the running Thread. Protected demand means runnable and eligible;
/// a Thread which voluntarily exhausted its own Budget is not owed execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Demand {
    pub protected: bool,
    /// Any non-Budget runnable work, including a throttled priority FIFO.
    pub fair: bool,
    /// Demand whose eligibility can change at the next CPU window.
    pub window_sensitive: bool,
    pub eligible_budget: bool,
    pub any_budget: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Candidate {
    pub parameters: SchedulingParameters,
    pub fair_pass: u128,
    pub turn: u64,
    pub budget_available: bool,
}

/// A selector supplies class-owned candidates; this key never owns a Thread.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DispatchKey {
    pub urgency: u8,
    pub rank: u8,
    /// A shared admission tier may contain different algorithms. Order them
    /// explicitly before comparing their incompatible local key units.
    pub algorithm_rank: u8,
    pub algorithm: u128,
    pub band: u8,
    pub turn: u64,
}

/// Why an execution interval was admitted. Priority-role fallback execution
/// still consumes allowance, but must not cause an immediate expired deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchMode {
    StrictFifo,
    Priority,
    Protected,
    Ordinary,
    Borrow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CpuWindow {
    config: ArbitrationConfig,
    epoch: Option<u64>,
    priority_remaining_ns: u64,
    protected_remaining_ns: u64,
}

impl CpuWindow {
    pub(crate) const fn new(config: ArbitrationConfig) -> Self {
        Self {
            config,
            epoch: None,
            priority_remaining_ns: config.priority_allowance_ns,
            protected_remaining_ns: config.protected_floor_ns,
        }
    }

    pub(crate) const fn period_ns(self) -> u64 {
        self.config.period_ns
    }

    #[cfg(test)]
    pub(crate) const fn priority_remaining_ns(self) -> u64 {
        self.priority_remaining_ns
    }
    #[cfg(test)]
    pub(crate) const fn protected_remaining_ns(self) -> u64 {
        self.protected_remaining_ns
    }

    pub(crate) fn refresh(&mut self, now: u64) {
        let epoch = now / self.config.period_ns;
        if self.epoch.is_none_or(|previous| epoch > previous) {
            self.epoch = Some(epoch);
            self.priority_remaining_ns = self.config.priority_allowance_ns;
            self.protected_remaining_ns = self.config.protected_floor_ns;
        }
    }

    /// The caller closes intervals before changing current identity or policy.
    /// A long sole borrower may span windows; only its final-window service
    /// debits the new ledger. No historical unused allowance accumulates.
    #[cfg(test)]
    pub(crate) fn account_interval(
        &mut self,
        start: u64,
        end: u64,
        parameters: SchedulingParameters,
    ) -> bool {
        let mode = if parameters.role() == SchedulingRole::Priority {
            DispatchMode::Priority
        } else {
            DispatchMode::Ordinary
        };
        self.account_dispatch(start, end, parameters, mode)
    }

    pub(crate) fn account_dispatch(
        &mut self,
        start: u64,
        end: u64,
        parameters: SchedulingParameters,
        mode: DispatchMode,
    ) -> bool {
        if end < start {
            return false;
        }
        self.refresh(end);
        let window_start = end - end % self.config.period_ns;
        let charge = end - start.max(window_start);
        if mode == DispatchMode::Priority || parameters.role() == SchedulingRole::Priority {
            self.priority_remaining_ns = self.priority_remaining_ns.saturating_sub(charge);
        }
        if parameters.role() == SchedulingRole::Protected {
            self.protected_remaining_ns = self.protected_remaining_ns.saturating_sub(charge);
        }
        true
    }

    pub(crate) fn period_deadline(&self, now: u64) -> Option<u64> {
        (now - now % self.config.period_ns).checked_add(self.config.period_ns)
    }

    pub(crate) fn protection_deadline(&mut self, now: u64, protected_waiting: bool) -> Option<u64> {
        self.refresh(now);
        if !protected_waiting || self.protected_remaining_ns == 0 {
            return None;
        }
        Some(
            self.period_deadline(now)?
                .saturating_sub(self.protected_remaining_ns)
                .max(now),
        )
    }

    pub(crate) fn candidate_key(
        &mut self,
        now: u64,
        candidate: Candidate,
        demand: Demand,
    ) -> Option<(DispatchKey, DispatchMode)> {
        self.refresh(now);
        let parameters = candidate.parameters;
        if !self.config.accepts(parameters) || parameters.policy() == SchedulingPolicy::Idle {
            return None;
        }
        let exhausted =
            parameters.policy() == SchedulingPolicy::Budget && !candidate.budget_available;
        if exhausted && (demand.fair || demand.eligible_budget) {
            return None;
        }
        let strict_fifo = matches!(parameters.policy(), SchedulingPolicy::Fifo { .. })
            && parameters.role() == SchedulingRole::Ordinary;
        let protected_due = self.protection_deadline(now, demand.protected) == Some(now);
        let (urgency, rank, mode) = if strict_fifo {
            (0, 0, DispatchMode::StrictFifo)
        } else if protected_due && parameters.role() == SchedulingRole::Protected && !exhausted {
            (1, 0, DispatchMode::Protected)
        } else if exhausted {
            (2, self.config.order.borrow_rank, DispatchMode::Borrow)
        } else if (parameters.role() == SchedulingRole::Priority
            || (self.config.prefer_fair && parameters.policy() == SchedulingPolicy::Fair))
            && self.priority_remaining_ns != 0
        {
            (2, self.config.order.priority_rank, DispatchMode::Priority)
        } else if parameters.policy() == SchedulingPolicy::Budget {
            (2, self.config.order.budget_rank, DispatchMode::Ordinary)
        } else {
            (2, self.config.order.fair_rank, DispatchMode::Ordinary)
        };
        let algorithm_rank = match parameters.policy() {
            SchedulingPolicy::Fifo { .. } => 0,
            SchedulingPolicy::Fair => 1,
            SchedulingPolicy::Budget => 2,
            SchedulingPolicy::Idle => return None,
        };
        let algorithm = match parameters.policy() {
            SchedulingPolicy::Fifo { priority } => u128::from(priority.get()),
            SchedulingPolicy::Fair if parameters.fair_backend() == super::FairBackend::Stride => {
                candidate.fair_pass
            }
            _ => u128::from(candidate.turn),
        };
        Some((
            DispatchKey {
                urgency,
                rank,
                algorithm_rank,
                band: parameters.band().get(),
                algorithm,
                turn: candidate.turn,
            },
            mode,
        ))
    }

    /// No contention means no arbitration interrupt, including at replenishment
    /// boundaries. A remote enqueue must therefore prompt the owning CPU to
    /// recalculate this deadline even when immediate preemption is unnecessary.
    pub(crate) fn next_deadline(
        &mut self,
        now: u64,
        mode: DispatchMode,
        competition: bool,
        demand: Demand,
    ) -> Option<u64> {
        self.refresh(now);
        if !competition || mode == DispatchMode::StrictFifo {
            return None;
        }
        let mut deadline = if demand.any_budget || demand.window_sensitive {
            self.period_deadline(now)
        } else {
            None
        };
        if mode == DispatchMode::Priority {
            deadline = earliest(deadline, now.checked_add(self.priority_remaining_ns));
        }
        if mode == DispatchMode::Protected {
            deadline = earliest(deadline, now.checked_add(self.protected_remaining_ns));
        } else {
            deadline = earliest(deadline, self.protection_deadline(now, demand.protected));
        }
        deadline
    }
}

fn earliest(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
