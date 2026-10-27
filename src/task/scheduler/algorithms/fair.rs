// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stride execution accounting shared with the optional FIFO-order RR backend.

use super::super::policy::ThreadWeight;

const SCALE: u128 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FairRuntime {
    pass: u128,
    remainder: u16,
    denominator: u16,
    remaining_ns: u64,
}

impl FairRuntime {
    pub(crate) const fn new(quantum_ns: u64) -> Self {
        Self {
            pass: 0,
            remainder: 0,
            denominator: 1,
            remaining_ns: quantum_ns,
        }
    }

    pub(crate) const fn pass(self) -> u128 {
        self.pass
    }
    pub(crate) const fn remaining_ns(self) -> u64 {
        self.remaining_ns
    }

    /// Preserve fractional charges across short runs; otherwise repeated
    /// blocking could avoid billing all executions shorter than one stride.
    pub(crate) fn account(&mut self, elapsed_ns: u64, weight: ThreadWeight) {
        let denominator = weight.get();
        let remainder =
            u128::from(self.remainder) * u128::from(denominator) / u128::from(self.denominator);
        let charge = u128::from(elapsed_ns) * SCALE + remainder;
        self.pass = self.pass.saturating_add(charge / u128::from(denominator));
        self.remainder = (charge % u128::from(denominator)) as u16;
        self.denominator = denominator;
        self.remaining_ns = self.remaining_ns.saturating_sub(elapsed_ns);
    }

    /// A new or long-sleeping Thread cannot retain unbounded sleeper credit.
    pub(crate) fn admit(&mut self, floor: u128) {
        if self.pass < floor {
            self.pass = floor;
            self.remainder = 0;
        }
    }

    /// Translate the execution debt to another CPU's virtual origin. Call only
    /// as part of an exact stopped-context migration, not an ordinary wake.
    pub(crate) fn migrate_floor(&mut self, old_floor: u128, new_floor: u128) {
        self.pass = if self.pass >= old_floor {
            new_floor.saturating_add(self.pass - old_floor)
        } else {
            new_floor.saturating_sub(old_floor - self.pass)
        };
    }

    pub(crate) fn finish_turn(&mut self, quantum_ns: u64) {
        self.remaining_ns = quantum_ns;
    }

    /// Parameter edits cannot replenish the current turn. A shorter quantum
    /// includes execution already charged under the previous configuration.
    pub(crate) fn reconfigure(&mut self, old_quantum_ns: u64, new_quantum_ns: u64) {
        let consumed = old_quantum_ns.saturating_sub(self.remaining_ns);
        self.remaining_ns = self
            .remaining_ns
            .min(new_quantum_ns.saturating_sub(consumed));
    }

    pub(crate) fn expire_turn(&mut self) {
        self.remaining_ns = 0;
    }
}
