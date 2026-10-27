// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lock-protected admission and residency state for immutable machine roots.

use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_OWNER_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResidencyError {
    Busy,
    AlreadyActive,
    InvalidCpu,
    NotActive,
    Retired,
    StaleEpoch,
    SequenceExhausted,
    OwnerExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResidencyPhase {
    Open,
    Updating,
    Retiring,
    Retired,
}

pub struct AddressSpaceResidency<const CPUS: usize> {
    owner_nonce: NonZeroU64,
    epoch: u64,
    phase: ResidencyPhase,
    active: [bool; CPUS],
    resident: [bool; CPUS],
    update_sequence: u64,
}

impl<const CPUS: usize> AddressSpaceResidency<CPUS> {
    pub fn try_new(epoch: u64) -> Result<Self, ResidencyError> {
        // The counter establishes identity only; state publication and cut
        // transitions remain protected by the owning address-space lock.
        let owner_nonce = NEXT_OWNER_NONCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |nonce| {
                nonce.checked_add(1).filter(|next| *next != 0)
            })
            .map_err(|_| ResidencyError::OwnerExhausted)?;
        let owner_nonce = NonZeroU64::new(owner_nonce).ok_or(ResidencyError::OwnerExhausted)?;
        Ok(Self {
            owner_nonce,
            epoch,
            phase: ResidencyPhase::Open,
            active: [false; CPUS],
            resident: [false; CPUS],
            update_sequence: 0,
        })
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn check_admission(&self, cpu: usize, epoch: u64) -> Result<(), ResidencyError> {
        if cpu >= CPUS {
            return Err(ResidencyError::InvalidCpu);
        }
        self.ensure_open()?;
        if epoch != self.epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        if self.active[cpu] {
            return Err(ResidencyError::AlreadyActive);
        }
        Ok(())
    }

    /// Publishes an admission after the caller installed the root while still
    /// holding the same external lock used for [`Self::check_admission`].
    pub fn publish_admission(&mut self, cpu: usize) -> Result<(), ResidencyError> {
        if cpu >= CPUS {
            return Err(ResidencyError::InvalidCpu);
        }
        self.ensure_open()?;
        if self.active[cpu] {
            return Err(ResidencyError::AlreadyActive);
        }
        self.active[cpu] = true;
        self.resident[cpu] = true;
        Ok(())
    }

    /// Checks the caller's residency without excluding other active CPUs.
    pub fn check_active(&self, cpu: usize, epoch: u64) -> Result<(), ResidencyError> {
        self.ensure_open()?;
        if epoch != self.epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        match self.active.get(cpu) {
            None => Err(ResidencyError::InvalidCpu),
            Some(false) => Err(ResidencyError::NotActive),
            Some(true) => Ok(()),
        }
    }

    /// Commits an epoch after architecture-wide mapping publication completes.
    /// The external lock excludes admissions and mutations; the caller must
    /// complete required invalidations on every resident CPU before this call.
    pub fn advance_shared_active(
        &mut self,
        cpu: usize,
        expected: u64,
        next: u64,
    ) -> Result<(), ResidencyError> {
        self.check_active(cpu, expected)?;
        if next <= expected {
            return Err(ResidencyError::StaleEpoch);
        }
        self.epoch = next;
        Ok(())
    }

    pub fn check_single_active(&self, cpu: usize, epoch: u64) -> Result<(), ResidencyError> {
        self.ensure_open()?;
        if self.epoch != epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        if cpu >= CPUS {
            return Err(ResidencyError::InvalidCpu);
        }
        if !self.active[cpu] {
            return Err(ResidencyError::NotActive);
        }
        if self
            .active
            .iter()
            .copied()
            .enumerate()
            .any(|(index, active)| index != cpu && active)
        {
            return Err(ResidencyError::Busy);
        }
        Ok(())
    }

    pub fn check_inactive(&self, epoch: u64) -> Result<(), ResidencyError> {
        self.ensure_open()?;
        if self.epoch != epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        if self.active.iter().copied().any(|active| active) {
            return Err(ResidencyError::Busy);
        }
        Ok(())
    }

    /// Removes one active CPU only after the update gate is open and the
    /// caller's observed machine epoch is current.
    pub fn leave(&mut self, cpu: usize, expected_epoch: u64) -> Result<(), ResidencyError> {
        self.ensure_open()?;
        if expected_epoch != self.epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        let active = self.active.get_mut(cpu).ok_or(ResidencyError::InvalidCpu)?;
        if !*active {
            return Err(ResidencyError::NotActive);
        }
        *active = false;
        Ok(())
    }

    pub fn begin_update(&mut self, expected_epoch: u64) -> Result<UpdateCut<CPUS>, ResidencyError> {
        self.ensure_open()?;
        if self.epoch != expected_epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        let sequence = self
            .update_sequence
            .checked_add(1)
            .ok_or(ResidencyError::SequenceExhausted)?;
        self.update_sequence = sequence;
        self.phase = ResidencyPhase::Updating;
        Ok(UpdateCut {
            owner_nonce: self.owner_nonce,
            base_epoch: self.epoch,
            sequence,
            active: self.active,
            targets: self.resident,
        })
    }

    /// Permanently closes admission while the owner retires every resident
    /// translation. The surrounding address-space owner is consumed, so no
    /// corresponding reopen transition exists.
    pub fn begin_retirement(
        &mut self,
        expected_epoch: u64,
    ) -> Result<RetirementCut<CPUS>, ResidencyError> {
        self.ensure_open()?;
        if self.active.iter().copied().any(|active| active) {
            return Err(ResidencyError::Busy);
        }
        if self.epoch != expected_epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        let sequence = self
            .update_sequence
            .checked_add(1)
            .ok_or(ResidencyError::SequenceExhausted)?;
        self.update_sequence = sequence;
        self.phase = ResidencyPhase::Retiring;
        Ok(RetirementCut {
            owner_nonce: self.owner_nonce,
            base_epoch: self.epoch,
            sequence,
            targets: self.resident,
        })
    }

    pub fn abort_update(
        &mut self,
        cut: UpdateCut<CPUS>,
    ) -> Result<(), CutFailure<UpdateCut<CPUS>>> {
        if let Err(error) = self.validate_update_cut(&cut) {
            return Err(CutFailure { error, cut });
        }
        self.phase = ResidencyPhase::Open;
        Ok(())
    }

    /// Opens admission for the new epoch after every cut target acknowledged.
    pub fn finish_update(
        &mut self,
        cut: UpdateCut<CPUS>,
        epoch: u64,
    ) -> Result<(), CutFailure<UpdateCut<CPUS>>> {
        if let Err(error) = self.validate_update_cut(&cut) {
            return Err(CutFailure { error, cut });
        }
        if epoch <= self.epoch {
            return Err(CutFailure {
                error: ResidencyError::StaleEpoch,
                cut,
            });
        }
        self.epoch = epoch;
        // Inactive residents were invalidated by the cut. Active targets have
        // installed the new root and remain resident.
        self.resident = self.active;
        self.phase = ResidencyPhase::Open;
        Ok(())
    }

    /// Permanently closes this residency after every retirement target has
    /// acknowledged invalidating the retained translation identity.
    pub fn finish_retirement(
        &mut self,
        cut: RetirementCut<CPUS>,
    ) -> Result<(), CutFailure<RetirementCut<CPUS>>> {
        if self.phase != ResidencyPhase::Retiring
            || cut.owner_nonce != self.owner_nonce
            || cut.base_epoch != self.epoch
            || cut.sequence != self.update_sequence
        {
            return Err(CutFailure {
                error: ResidencyError::StaleEpoch,
                cut,
            });
        }
        self.active = [false; CPUS];
        self.resident = [false; CPUS];
        self.phase = ResidencyPhase::Retired;
        Ok(())
    }

    /// Advances an in-place mapping epoch while exactly one admitted CPU owns
    /// and locally invalidates the mutable translation hierarchy.
    pub fn advance_single_active(
        &mut self,
        cpu: usize,
        expected_epoch: u64,
        new_epoch: u64,
    ) -> Result<(), ResidencyError> {
        self.check_single_active(cpu, expected_epoch)?;
        if new_epoch <= expected_epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        self.epoch = new_epoch;
        Ok(())
    }

    /// Advances an unpublished or quiescent hierarchy in place.
    pub fn advance_inactive(
        &mut self,
        expected_epoch: u64,
        new_epoch: u64,
    ) -> Result<(), ResidencyError> {
        self.check_inactive(expected_epoch)?;
        if new_epoch <= expected_epoch {
            return Err(ResidencyError::StaleEpoch);
        }
        self.epoch = new_epoch;
        Ok(())
    }

    pub const fn is_retired(&self) -> bool {
        matches!(self.phase, ResidencyPhase::Retired)
    }

    fn ensure_open(&self) -> Result<(), ResidencyError> {
        match self.phase {
            ResidencyPhase::Open => Ok(()),
            ResidencyPhase::Updating | ResidencyPhase::Retiring => Err(ResidencyError::Busy),
            ResidencyPhase::Retired => Err(ResidencyError::Retired),
        }
    }

    fn validate_update_cut(&self, cut: &UpdateCut<CPUS>) -> Result<(), ResidencyError> {
        if self.phase != ResidencyPhase::Updating
            || cut.owner_nonce != self.owner_nonce
            || cut.base_epoch != self.epoch
            || cut.sequence != self.update_sequence
        {
            return Err(ResidencyError::StaleEpoch);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct UpdateCut<const CPUS: usize> {
    owner_nonce: NonZeroU64,
    base_epoch: u64,
    sequence: u64,
    active: [bool; CPUS],
    targets: [bool; CPUS],
}

impl<const CPUS: usize> UpdateCut<CPUS> {
    pub const fn active(&self) -> &[bool; CPUS] {
        &self.active
    }

    pub const fn targets(&self) -> &[bool; CPUS] {
        &self.targets
    }

    pub const fn base_epoch(&self) -> u64 {
        self.base_epoch
    }
}

/// Irreversible target snapshot for final address-space retirement.
///
/// Unlike [`UpdateCut`], this token has no abort operation and cannot be
/// supplied to an ordinary update completion API.
#[derive(Debug)]
pub struct RetirementCut<const CPUS: usize> {
    owner_nonce: NonZeroU64,
    base_epoch: u64,
    sequence: u64,
    targets: [bool; CPUS],
}

impl<const CPUS: usize> RetirementCut<CPUS> {
    pub const fn targets(&self) -> &[bool; CPUS] {
        &self.targets
    }

    pub const fn base_epoch(&self) -> u64 {
        self.base_epoch
    }
}

#[derive(Debug)]
pub struct CutFailure<Cut> {
    error: ResidencyError,
    cut: Cut,
}

impl<Cut> CutFailure<Cut> {
    pub const fn error(&self) -> ResidencyError {
        self.error
    }

    pub fn into_cut(self) -> Cut {
        self.cut
    }
}
