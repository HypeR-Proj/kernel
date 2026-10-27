// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lazy fixed-window budgets; idle borrowing never creates future debt.

use super::BudgetParameters;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BudgetRuntime {
    epoch: Option<u64>,
    remaining_ns: u64,
    burst_remaining_ns: u64,
}

impl BudgetRuntime {
    pub(crate) const fn new(parameters: BudgetParameters) -> Self {
        Self {
            epoch: None,
            remaining_ns: parameters.budget_ns(),
            burst_remaining_ns: parameters.burst_ns(),
        }
    }

    pub(crate) const fn remaining_ns(self) -> u64 {
        self.remaining_ns
    }
    pub(crate) const fn burst_remaining_ns(self) -> u64 {
        self.burst_remaining_ns
    }

    pub(crate) fn refresh(&mut self, now: u64, parameters: BudgetParameters) {
        let epoch = now / parameters.period_ns();
        if self.epoch.is_none_or(|previous| epoch > previous) {
            self.epoch = Some(epoch);
            self.remaining_ns = parameters.budget_ns();
        } else {
            // An authorized reduction takes effect without refreshing time.
            self.remaining_ns = self.remaining_ns.min(parameters.budget_ns());
        }
    }

    /// An uninterrupted idle borrower can cross many windows. Only execution
    /// in the final window affects its newly replenished budget; the complete
    /// interval still consumes its independent continuous-dispatch allowance.
    pub(crate) fn account_interval(
        &mut self,
        start: u64,
        end: u64,
        parameters: BudgetParameters,
    ) -> bool {
        let Some(elapsed) = end.checked_sub(start) else {
            return false;
        };
        self.refresh(end, parameters);
        let window_start = end - end % parameters.period_ns();
        let charge = end - start.max(window_start);
        self.remaining_ns = self.remaining_ns.saturating_sub(charge);
        self.burst_remaining_ns = self.burst_remaining_ns.saturating_sub(elapsed);
        true
    }

    pub(crate) fn period_deadline(now: u64, parameters: BudgetParameters) -> Option<u64> {
        (now - now % parameters.period_ns()).checked_add(parameters.period_ns())
    }

    pub(crate) fn deadline(
        &mut self,
        now: u64,
        parameters: BudgetParameters,
        borrow: bool,
    ) -> Option<u64> {
        self.refresh(now, parameters);
        if self.remaining_ns == 0 && !borrow {
            return Self::period_deadline(now, parameters);
        }
        let execution = if borrow && self.remaining_ns == 0 {
            self.burst_remaining_ns
        } else {
            self.remaining_ns.min(self.burst_remaining_ns)
        };
        let expiry = now.checked_add(execution)?;
        Some(expiry.min(Self::period_deadline(now, parameters)?))
    }

    pub(crate) fn finish_turn(&mut self, parameters: BudgetParameters) {
        self.burst_remaining_ns = parameters.burst_ns();
    }

    /// Changes ceilings without opening a new window or dispatch turn. Raised
    /// Q takes effect at the next period; raised B at the next real turn.
    /// Repeated decrease/increase operations therefore cannot mint execution.
    pub(crate) fn reconfigure(&mut self, old: BudgetParameters, new: BudgetParameters) -> bool {
        if old.period_ns() != new.period_ns() {
            return false;
        }
        let consumed = old.budget_ns().saturating_sub(self.remaining_ns);
        self.remaining_ns = self
            .remaining_ns
            .min(new.budget_ns().saturating_sub(consumed));
        let burst_consumed = old.burst_ns().saturating_sub(self.burst_remaining_ns);
        self.burst_remaining_ns = self
            .burst_remaining_ns
            .min(new.burst_ns().saturating_sub(burst_consumed));
        true
    }
}
