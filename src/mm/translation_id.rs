// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Generation-tagged allocation for hardware translation identifiers.
//!
//! This module is deliberately synchronization-free. A kernel policy owner
//! places a pool behind its chosen lock and retains the non-forgeable state
//! tokens through publication, invalidation, and retirement.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranslationIdError {
    Exhausted,
    InvalidToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum SlotState {
    Vacant,
    Reserved,
    Active,
    Retiring,
    Exhausted,
}

#[derive(Clone, Copy)]
struct Slot {
    generation: u64,
    state: SlotState,
}

impl Slot {
    const VACANT: Self = Self {
        generation: 1,
        state: SlotState::Vacant,
    };
}

/// Mutable state for one architectural identifier namespace.
///
/// Identifier zero is permanently withheld because both Arm ASID and VMID
/// users conventionally reserve it for boot or untagged contexts.
pub struct TranslationIdPool<Namespace, const COUNT: usize> {
    slots: [Slot; COUNT],
    namespace: core::marker::PhantomData<fn() -> Namespace>,
}

impl<Namespace, const COUNT: usize> TranslationIdPool<Namespace, COUNT> {
    /// Creates the sole pool for one hardware namespace marker.
    ///
    /// # Safety
    ///
    /// No other live `TranslationIdPool` may use the same `Namespace` marker.
    /// Tokens are branded by that marker, so duplicate pools would allow a
    /// token to transition the wrong pool and could duplicate a hardware ID.
    pub const unsafe fn new() -> Self {
        Self {
            slots: [Slot::VACANT; COUNT],
            namespace: core::marker::PhantomData,
        }
    }

    pub fn reserve(&mut self) -> Result<ReservedTranslationId<Namespace>, TranslationIdError> {
        self.reserve_below(COUNT)
    }

    /// Reserves a nonzero identifier strictly below `exclusive_limit`.
    pub fn reserve_below(
        &mut self,
        exclusive_limit: usize,
    ) -> Result<ReservedTranslationId<Namespace>, TranslationIdError> {
        let upper = COUNT.min(u16::MAX as usize + 1).min(exclusive_limit);
        let Some((index, slot)) = self
            .slots
            .iter_mut()
            .enumerate()
            .take(upper)
            .skip(1)
            .find(|(_, slot)| slot.state == SlotState::Vacant)
        else {
            return Err(TranslationIdError::Exhausted);
        };
        slot.state = SlotState::Reserved;
        Ok(ReservedTranslationId {
            value: index as u16,
            generation: slot.generation,
            namespace: core::marker::PhantomData,
        })
    }

    pub fn cancel(
        &mut self,
        reservation: ReservedTranslationId<Namespace>,
    ) -> Result<(), TranslationIdError> {
        let slot = self.match_slot(reservation.value, reservation.generation)?;
        if slot.state != SlotState::Reserved {
            return Err(TranslationIdError::InvalidToken);
        }
        advance(slot);
        Ok(())
    }

    pub fn activate(
        &mut self,
        reservation: ReservedTranslationId<Namespace>,
    ) -> Result<ActiveTranslationId<Namespace>, TranslationIdError> {
        let slot = self.match_slot(reservation.value, reservation.generation)?;
        if slot.state != SlotState::Reserved {
            return Err(TranslationIdError::InvalidToken);
        }
        slot.state = SlotState::Active;
        Ok(ActiveTranslationId {
            value: reservation.value,
            generation: reservation.generation,
            namespace: core::marker::PhantomData,
        })
    }

    /// Moves an active identifier into the state which forbids reuse while
    /// its final tagged invalidation is in flight.
    pub fn begin_retirement(
        &mut self,
        active: ActiveTranslationId<Namespace>,
    ) -> Result<RetiringTranslationId<Namespace>, TranslationIdError> {
        let slot = self.match_slot(active.value, active.generation)?;
        if slot.state != SlotState::Active {
            return Err(TranslationIdError::InvalidToken);
        }
        slot.state = SlotState::Retiring;
        Ok(RetiringTranslationId {
            value: active.value,
            generation: active.generation,
            namespace: core::marker::PhantomData,
        })
    }

    /// Makes an identifier reusable after its invalidation acknowledgement.
    ///
    /// # Safety
    ///
    /// Every processing element which could cache this identifier must have
    /// completed the architecture-specific tagged invalidation. A caller must
    /// fail-stop instead of invoking this method after an ambiguous shootdown.
    pub unsafe fn complete_retirement(
        &mut self,
        retiring: RetiringTranslationId<Namespace>,
    ) -> Result<(), TranslationIdError> {
        let slot = self.match_slot(retiring.value, retiring.generation)?;
        if slot.state != SlotState::Retiring {
            return Err(TranslationIdError::InvalidToken);
        }
        advance(slot);
        Ok(())
    }

    fn match_slot(&mut self, value: u16, generation: u64) -> Result<&mut Slot, TranslationIdError> {
        let slot = self
            .slots
            .get_mut(value as usize)
            .ok_or(TranslationIdError::InvalidToken)?;
        if value == 0 || slot.generation != generation {
            return Err(TranslationIdError::InvalidToken);
        }
        Ok(slot)
    }
}

fn advance(slot: &mut Slot) {
    match slot.generation.checked_add(1) {
        Some(generation) => {
            slot.generation = generation;
            slot.state = SlotState::Vacant;
        }
        None => slot.state = SlotState::Exhausted,
    }
}

#[must_use = "an unpublished identifier must be activated or cancelled"]
pub struct ReservedTranslationId<Namespace> {
    value: u16,
    generation: u64,
    namespace: core::marker::PhantomData<fn() -> Namespace>,
}

impl<Namespace> ReservedTranslationId<Namespace> {
    pub const fn value(&self) -> u16 {
        self.value
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[must_use = "an active identifier must remain retained or enter acknowledged retirement"]
pub struct ActiveTranslationId<Namespace> {
    value: u16,
    generation: u64,
    namespace: core::marker::PhantomData<fn() -> Namespace>,
}

impl<Namespace> ActiveTranslationId<Namespace> {
    pub const fn value(&self) -> u16 {
        self.value
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[must_use = "a retiring identifier must remain retained until invalidation is acknowledged"]
pub struct RetiringTranslationId<Namespace> {
    value: u16,
    generation: u64,
    namespace: core::marker::PhantomData<fn() -> Namespace>,
}

impl<Namespace> RetiringTranslationId<Namespace> {
    pub const fn value(&self) -> u16 {
        self.value
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}
