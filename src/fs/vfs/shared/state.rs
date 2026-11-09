// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free admission and dirty generation state, under the page lock.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Active,
    Freezing,
    Retired,
    Failed,
}

pub(super) struct State {
    pub(super) phase: Phase,
    pub(super) epoch: u64,
    pub(super) dirty: bool,
    writers: usize,
    installers: usize,
    pins: usize,
}

impl State {
    pub(super) const fn new() -> Self {
        Self {
            phase: Phase::Active,
            epoch: 1,
            dirty: false,
            writers: 0,
            installers: 0,
            pins: 0,
        }
    }
    pub(super) fn pin(&mut self) -> bool {
        if self.phase != Phase::Active {
            return false;
        }
        let Some(next) = self.pins.checked_add(1) else {
            return false;
        };
        self.pins = next;
        true
    }
    pub(super) fn unpin(&mut self) {
        assert!(self.pins != 0, "shared page pin underflow");
        self.pins -= 1;
    }
    pub(super) fn close_for_eviction(&mut self) -> Option<bool> {
        if self.pins != 0 {
            return None;
        }
        Some(self.close())
    }
    /// Demand preparation may own earlier installers itself. Pressure work
    /// must skip those owners instead of waiting for its own stack to unwind.
    pub(super) fn try_close_for_reclaim(&mut self) -> bool {
        match self.close_for_eviction() {
            Some(true) => true,
            Some(false) => {
                self.reopen();
                false
            }
            None => false,
        }
    }
    pub(super) fn admit(&mut self, write: bool) -> Option<u64> {
        if self.phase != Phase::Active {
            return None;
        }
        let count = if write {
            &mut self.writers
        } else {
            &mut self.installers
        };
        *count = count.checked_add(1)?;
        if write {
            self.dirty = true;
        }
        Some(self.epoch)
    }
    pub(super) fn release(&mut self, write: bool) {
        let count = if write {
            &mut self.writers
        } else {
            &mut self.installers
        };
        assert!(*count != 0, "shared page admission underflow");
        *count -= 1;
    }
    pub(super) fn valid(&self, epoch: u64, write: bool) -> bool {
        self.epoch == epoch
            && (self.phase == Phase::Active || (write && self.phase == Phase::Freezing))
    }
    /// Closing admission precedes checking existing short copies/installers.
    pub(super) fn close(&mut self) -> bool {
        if self.phase == Phase::Active {
            self.phase = Phase::Freezing;
        }
        matches!(self.phase, Phase::Freezing | Phase::Retired | Phase::Failed)
            && self.writers == 0
            && self.installers == 0
    }
    pub(super) fn reopen(&mut self) {
        if self.phase == Phase::Freezing {
            self.phase = Phase::Active;
        }
    }
    /// Failed submission returns ownership to the latest canonical contents.
    /// The successor epoch remains intact: a concurrent newer writer is never
    /// acknowledged by the failed old snapshot. A failed eviction kept its
    /// resident owner and may retry after local pre-submit pressure clears.
    pub(super) fn submission_failed(&mut self) {
        self.dirty = true;
        if self.phase == Phase::Retired {
            self.phase = Phase::Active;
        }
    }
    /// The private snapshot owns this generation. Later grants redirty the
    /// successor; backend completion must not clear it again.
    pub(super) fn captured(&mut self, retire: bool) -> Option<u64> {
        if self.phase != Phase::Freezing || self.writers != 0 || self.installers != 0 {
            return None;
        }
        let next = self.epoch.checked_add(1)?;
        let captured = self.epoch;
        self.epoch = next;
        self.dirty = false;
        self.phase = if retire {
            Phase::Retired
        } else {
            Phase::Active
        };
        Some(captured)
    }
}
