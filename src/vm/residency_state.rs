// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure identity and per-CPU observation model for guest stage-2 residency.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Stage2AllocationIdentity {
    root: u64,
    generation: u64,
}

#[cfg_attr(test, allow(dead_code))]
impl Stage2AllocationIdentity {
    pub(super) const fn new(root: u64, generation: u64) -> Self {
        Self { root, generation }
    }

    pub(super) const fn root(self) -> u64 {
        self.root
    }

    pub(super) const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Stage2Incarnation {
    allocation: Stage2AllocationIdentity,
    translation_epoch: u64,
}

#[cfg_attr(test, allow(dead_code))]
impl Stage2Incarnation {
    pub(super) const fn new(root: u64, generation: u64, translation_epoch: u64) -> Self {
        Self {
            allocation: Stage2AllocationIdentity::new(root, generation),
            translation_epoch,
        }
    }

    pub(super) const fn allocation(self) -> Stage2AllocationIdentity {
        self.allocation
    }

    pub(super) const fn translation_epoch(self) -> u64 {
        self.translation_epoch
    }

    pub(super) const fn same_allocation(self, other: Self) -> bool {
        self.allocation.root == other.allocation.root
            && self.allocation.generation == other.allocation.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LocalStage2Observation {
    allocation: Stage2AllocationIdentity,
    translation_epoch: u64,
    synchronization_epoch: u64,
}

#[cfg_attr(test, allow(dead_code))]
impl LocalStage2Observation {
    pub(super) const EMPTY: Self = Self {
        allocation: Stage2AllocationIdentity::new(0, 0),
        translation_epoch: 0,
        synchronization_epoch: 0,
    };

    pub(super) const fn new(incarnation: Stage2Incarnation, synchronization_epoch: u64) -> Self {
        Self {
            allocation: incarnation.allocation,
            translation_epoch: incarnation.translation_epoch,
            synchronization_epoch,
        }
    }

    pub(super) const fn matches(
        self,
        incarnation: Stage2Incarnation,
        synchronization_epoch: u64,
    ) -> bool {
        self.allocation.root == incarnation.allocation.root
            && self.allocation.generation == incarnation.allocation.generation
            && self.translation_epoch == incarnation.translation_epoch
            && self.synchronization_epoch == synchronization_epoch
    }

    /// Clears only an observation owned by the exact VMID allocation and root.
    /// Translation epochs may differ because inactive CPUs intentionally retain
    /// historical observations until final retirement invalidates the VMID.
    pub(super) fn clear_allocation(&mut self, allocation: Stage2AllocationIdentity) -> bool {
        if self.allocation != allocation {
            return false;
        }
        *self = Self::EMPTY;
        true
    }

    pub(super) const fn allocation(self) -> Stage2AllocationIdentity {
        self.allocation
    }

    pub(super) const fn translation_epoch(self) -> u64 {
        self.translation_epoch
    }

    pub(super) const fn synchronization_epoch(self) -> u64 {
        self.synchronization_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalStage2Observation, Stage2AllocationIdentity, Stage2Incarnation};

    #[test]
    fn generation_prevents_root_and_epoch_aba() {
        let old = Stage2Incarnation::new(0x4000, 11, 1);
        let reused = Stage2Incarnation::new(0x4000, 12, 1);
        let observed = LocalStage2Observation::new(old, 3);
        assert!(observed.matches(old, 3));
        assert!(!observed.matches(reused, 3));
    }

    #[test]
    fn exact_allocation_clear_preserves_foreign_observations() {
        let incarnation = Stage2Incarnation::new(0x8000, 4, 6);
        let mut observed = LocalStage2Observation::new(incarnation, 12);
        assert!(!observed.clear_allocation(Stage2AllocationIdentity::new(0x8000, 5)));
        assert!(observed.matches(incarnation, 12));
        assert!(observed.clear_allocation(incarnation.allocation()));
        assert_eq!(observed, LocalStage2Observation::EMPTY);
    }

    #[test]
    fn mapping_epoch_advance_preserves_the_admission_allocation() {
        let admitted = Stage2Incarnation::new(0xc000, 8, 21);
        let current = Stage2Incarnation::new(0xc000, 8, 22);
        assert!(admitted.same_allocation(current));
        assert_ne!(admitted, current);
    }
}
