// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free advisory lock admission and owner lifecycle.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Admission {
    Granted,
    Wait,
    Busy,
    Closed,
}

/// Encoded atomically by the runtime, but only mutated under its domain lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OwnerState(u8);

impl OwnerState {
    pub(super) const fn new() -> Self {
        Self(0)
    }

    pub(super) fn from_bits(bits: u8) -> Option<Self> {
        (bits & !15 == 0 && bits & 3 != 3 && (bits & 4 == 0 || bits & 3 == 0)).then_some(Self(bits))
    }

    pub(super) const fn bits(self) -> u8 {
        self.0
    }

    pub(super) const fn held(self) -> Option<LockMode> {
        match self.0 & 3 {
            1 => Some(LockMode::Shared),
            2 => Some(LockMode::Exclusive),
            _ => None,
        }
    }

    pub(super) const fn closed(self) -> bool {
        self.0 & 4 != 0
    }

    pub(super) const fn pending(self) -> bool {
        self.0 & 8 != 0
    }

    pub(super) fn set_pending(&mut self, pending: bool) {
        self.0 = (self.0 & !8) | if pending { 8 } else { 0 };
    }

    fn set_held(&mut self, mode: Option<LockMode>) {
        self.0 = (self.0 & !3)
            | match mode {
                None => 0,
                Some(LockMode::Shared) => 1,
                Some(LockMode::Exclusive) => 2,
            };
    }
}

pub(super) struct Grants {
    shared: usize,
    exclusive: bool,
}

impl Grants {
    pub(super) const fn new() -> Self {
        Self {
            shared: 0,
            exclusive: false,
        }
    }

    pub(super) fn compatible(&self, mode: LockMode) -> bool {
        !self.exclusive && (mode == LockMode::Shared || self.shared == 0)
    }

    pub(super) fn classify(&self, owner: &OwnerState, mode: LockMode, queued: bool) -> Admission {
        if owner.closed() {
            return Admission::Closed;
        }
        if owner.pending() {
            return Admission::Busy;
        }
        if owner.held() == Some(mode) {
            return Admission::Granted;
        }
        match owner.held() {
            Some(LockMode::Exclusive) => Admission::Granted,
            Some(LockMode::Shared) => {
                if self.shared != 1 || queued {
                    return Admission::Busy;
                }
                Admission::Granted
            }
            None if !queued && self.compatible(mode) => Admission::Granted,
            None => Admission::Wait,
        }
    }

    pub(super) fn request(
        &mut self,
        owner: &mut OwnerState,
        mode: LockMode,
        queued: bool,
    ) -> Admission {
        let admission = self.classify(owner, mode, queued);
        if admission == Admission::Granted && owner.held() != Some(mode) {
            self.release(owner);
            self.grant(owner, mode);
        }
        admission
    }

    /// Called only after admission or the exact scheduler notification wins.
    pub(super) fn grant(&mut self, owner: &mut OwnerState, mode: LockMode) {
        match mode {
            LockMode::Shared => self.shared += 1,
            LockMode::Exclusive => self.exclusive = true,
        }
        owner.set_held(Some(mode));
    }

    pub(super) fn release(&mut self, owner: &mut OwnerState) {
        match owner.held() {
            Some(LockMode::Shared) => self.shared -= 1,
            Some(LockMode::Exclusive) => self.exclusive = false,
            None => {}
        }
        owner.set_held(None);
    }

    pub(super) fn close(&mut self, owner: &mut OwnerState) {
        self.release(owner);
        owner.0 |= 4;
    }
}
