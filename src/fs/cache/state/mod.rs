// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-bounded state model for the immutable file-data cache.

use alloc::vec::Vec;
use core::alloc::Layout;

use super::{CacheKey, index};

mod eviction;

use eviction::EvictionHeap;

#[cfg(test)]
#[path = "../../../../tests/host/src/cases/file_cache_state.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Allocation,
    InvalidCapacity,
    SequenceExhausted,
    StaleLoad,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LoadToken {
    slot: usize,
    generation: u64,
    key: CacheKey,
    access_sequence: u64,
}

impl LoadToken {
    pub(super) const fn slot(self) -> usize {
        self.slot
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Access {
    Hit { slot: usize },
    LoadInProgress,
    Reserved { token: LoadToken, evicted: bool },
    CapacityBusy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(super) struct Snapshot {
    pub(super) capacity: usize,
    pub(super) clean: usize,
    pub(super) loading: usize,
    pub(super) hits: u64,
    pub(super) misses: u64,
    pub(super) evictions: u64,
    pub(super) in_progress: u64,
    pub(super) capacity_busy: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Vacant,
    Loading { key: CacheKey, generation: u64 },
    Clean { key: CacheKey, last_access: u64 },
}

impl Slot {
    fn key(self) -> Option<CacheKey> {
        match self {
            Self::Vacant => None,
            Self::Loading { key, .. } | Self::Clean { key, .. } => Some(key),
        }
    }
}

pub(super) struct State {
    slots: Vec<Slot>,
    index: index::Index,
    eviction: EvictionHeap,
    next_load_generation: u64,
    next_access_sequence: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
    in_progress: u64,
    capacity_busy: u64,
    clean: usize,
    loading: usize,
}

impl State {
    pub(super) fn try_new(capacity: usize) -> Result<Self, Error> {
        let index = index::Index::try_new(capacity)?;
        let eviction = EvictionHeap::try_new(capacity)?;
        let mut slots = super::memory::vector(capacity)?;
        slots.resize(capacity, Slot::Vacant);
        Ok(Self {
            slots,
            index,
            eviction,
            next_load_generation: 0,
            next_access_sequence: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            in_progress: 0,
            capacity_busy: 0,
            clean: 0,
            loading: 0,
        })
    }

    #[cfg(test)]
    pub(super) fn access(&mut self, key: CacheKey) -> Result<Access, Error> {
        self.access_with_admission(key, true)
    }

    #[cfg(test)]
    pub(super) fn access_with_admission(
        &mut self,
        key: CacheKey,
        allow_load: bool,
    ) -> Result<Access, Error> {
        self.access_with_victim(key, allow_load, |_| true)
    }

    pub(super) fn access_with_victim(
        &mut self,
        key: CacheKey,
        allow_load: bool,
        mut evictable: impl FnMut(usize) -> bool,
    ) -> Result<Access, Error> {
        if let Some(access) = self.find(key)? {
            return Ok(access);
        }
        if !allow_load || !self.index.can_insert(key) {
            self.record_capacity_busy();
            return Ok(Access::CapacityBusy);
        }

        let target = self.index.vacant().or_else(|| {
            // A bounded cold scan avoids spinning under the IRQ-safe cache lock
            // when mappings retain every page. Demand may grow the index.
            (0..self.clean.min(64))
                .filter_map(|index| self.eviction.candidate(index))
                .find(|slot| evictable(*slot))
        });
        let Some(slot) = target else {
            self.record_capacity_busy();
            return Ok(Access::CapacityBusy);
        };
        let generation = self.reserve_load_generation()?;
        let access_sequence = self.reserve_access_sequence()?;
        let Some(previous) = self.slots.get(slot).copied() else {
            index::invariant_failure();
        };
        let evicted = match previous {
            Slot::Vacant => {
                self.index.take_vacant(slot);
                false
            }
            Slot::Clean { key, .. } => {
                self.eviction.remove(slot);
                self.index.remove(key, slot);
                true
            }
            Slot::Loading { .. } => index::invariant_failure(),
        };
        self.slots[slot] = Slot::Loading { key, generation };
        self.index.insert(key, slot);
        self.loading += 1;
        self.misses = self.misses.saturating_add(1);
        if evicted {
            self.clean -= 1;
            self.evictions = self.evictions.saturating_add(1);
        }
        Ok(Access::Reserved {
            token: LoadToken {
                slot,
                generation,
                key,
                access_sequence,
            },
            evicted,
        })
    }

    /// Index-only lookup for a detached, immutable maintenance table.
    pub(super) fn peek_clean(&self, key: CacheKey) -> Option<usize> {
        self.index.find(key, |slot| matches!(self.slots.get(slot), Some(Slot::Clean { key: found, .. }) if *found == key))
    }

    pub(super) fn lookup(&mut self, key: CacheKey) -> Result<Option<usize>, Error> {
        Ok(match self.find(key)? {
            Some(Access::Hit { slot }) => Some(slot),
            _ => None,
        })
    }

    fn find(&mut self, key: CacheKey) -> Result<Option<Access>, Error> {
        let Some(slot) = self.index.find(key, |slot| {
            let Some(entry) = self.slots.get(slot).and_then(|slot| slot.key()) else {
                index::invariant_failure();
            };
            entry == key
        }) else {
            return Ok(None);
        };
        match self.slots[slot] {
            Slot::Clean { .. } => {
                let access_sequence = self.reserve_access_sequence()?;
                self.eviction.touch(slot, access_sequence);
                self.slots[slot] = Slot::Clean {
                    key,
                    last_access: access_sequence,
                };
                self.hits = self.hits.saturating_add(1);
                Ok(Some(Access::Hit { slot }))
            }
            Slot::Loading { .. } => {
                self.in_progress = self.in_progress.saturating_add(1);
                Ok(Some(Access::LoadInProgress))
            }
            Slot::Vacant => index::invariant_failure(),
        }
    }

    pub(super) fn record_capacity_busy(&mut self) {
        self.capacity_busy = self.capacity_busy.saturating_add(1);
    }

    pub(super) fn publish(&mut self, token: LoadToken) -> Result<(), Error> {
        self.validate(token)?;
        let Some(entry) = self.slots.get_mut(token.slot) else {
            return Err(Error::StaleLoad);
        };
        *entry = Slot::Clean {
            key: token.key,
            last_access: token.access_sequence,
        };
        self.eviction.insert(token.slot, token.access_sequence);
        self.loading -= 1;
        self.clean += 1;
        Ok(())
    }

    pub(super) fn abort(&mut self, token: LoadToken) -> Result<(), Error> {
        self.validate(token)?;
        self.index.remove(token.key, token.slot);
        self.slots[token.slot] = Slot::Vacant;
        self.index.release(token.slot);
        self.loading -= 1;
        Ok(())
    }

    pub(super) fn validate(&self, token: LoadToken) -> Result<(), Error> {
        let Some(entry) = self.slots.get(token.slot) else {
            return Err(Error::StaleLoad);
        };
        if *entry
            != (Slot::Loading {
                key: token.key,
                generation: token.generation,
            })
        {
            return Err(Error::StaleLoad);
        }
        Ok(())
    }

    pub(super) fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub(super) const fn clean(&self) -> usize {
        self.clean
    }

    pub(super) const fn loading(&self) -> usize {
        self.loading
    }

    pub(super) fn oldest_clean(&self) -> Option<usize> {
        self.eviction.oldest()
    }

    pub(super) fn cold_candidate(&self, index: usize) -> Option<usize> {
        self.eviction.candidate(index)
    }

    pub(super) fn remove_clean(&mut self, slot: usize) {
        self.detach_clean(slot);
        self.evictions = self.evictions.saturating_add(1);
    }

    /// Migration removes membership without counting an eviction.
    pub(super) fn detach_clean(&mut self, slot: usize) {
        let Some(Slot::Clean { key, .. }) = self.slots.get(slot).copied() else {
            index::invariant_failure();
        };
        self.eviction.remove(slot);
        self.index.remove(key, slot);
        self.slots[slot] = Slot::Vacant;
        self.index.release(slot);
        self.clean -= 1;
    }

    /// Only maintenance uses these entries, with the entire table detached
    /// from readers and no outstanding load reservations.
    pub(super) fn clean_entry(&self, slot: usize) -> Option<(CacheKey, u64)> {
        match self.slots.get(slot)? {
            Slot::Clean { key, last_access } => Some((*key, *last_access)),
            Slot::Vacant | Slot::Loading { .. } => None,
        }
    }

    pub(super) fn restore_clean(&mut self, key: CacheKey, last_access: u64) -> Option<usize> {
        // Shrinking merges old buckets. Keep migration best effort rather than
        // rebuilding an overlong chain from individually bounded old buckets.
        if !self.index.can_insert(key) {
            return None;
        }
        let slot = self.index.vacant()?;
        self.index.take_vacant(slot);
        self.index.insert(key, slot);
        self.eviction.insert(slot, last_access);
        self.slots[slot] = Slot::Clean { key, last_access };
        self.clean += 1;
        Some(slot)
    }

    pub(super) fn inherit_history(&mut self, old: &Self) {
        self.next_load_generation = old.next_load_generation;
        self.next_access_sequence = old.next_access_sequence;
        self.hits = old.hits;
        self.misses = old.misses;
        self.evictions = old.evictions;
        self.in_progress = old.in_progress;
        self.capacity_busy = old.capacity_busy;
    }

    pub(super) fn allocation_layouts(capacity: usize) -> Result<[Layout; 6], Error> {
        let [heads, next, free] = index::Index::allocation_layouts(capacity)?;
        let [entries, positions] = EvictionHeap::allocation_layouts(capacity)?;
        let slots = Layout::array::<Slot>(capacity).map_err(|_| Error::InvalidCapacity)?;
        Ok([slots, heads, next, free, entries, positions])
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Snapshot {
        let mut clean = 0;
        let mut loading = 0;
        for entry in &self.slots {
            match entry {
                Slot::Vacant => {}
                Slot::Loading { .. } => loading += 1,
                Slot::Clean { .. } => clean += 1,
            }
        }
        Snapshot {
            capacity: self.slots.len(),
            clean,
            loading,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            in_progress: self.in_progress,
            capacity_busy: self.capacity_busy,
        }
    }

    fn reserve_load_generation(&mut self) -> Result<u64, Error> {
        let generation = self
            .next_load_generation
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        self.next_load_generation = generation;
        Ok(generation)
    }

    fn reserve_access_sequence(&mut self) -> Result<u64, Error> {
        let sequence = self
            .next_access_sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        self.next_access_sequence = sequence;
        Ok(sequence)
    }

    #[cfg(test)]
    pub(super) fn invalidate_for_test(&mut self, slot: usize) {
        if let Some(key) = self.slots.get(slot).and_then(|slot| slot.key()) {
            if matches!(self.slots[slot], Slot::Clean { .. }) {
                self.eviction.remove(slot);
                self.clean -= 1;
            } else {
                self.loading -= 1;
            }
            self.index.remove(key, slot);
            self.slots[slot] = Slot::Vacant;
            self.index.release(slot);
        }
    }

    #[cfg(test)]
    pub(super) fn exhaust_sequences_for_test(&mut self) {
        self.next_access_sequence = u64::MAX;
        self.next_load_generation = u64::MAX;
    }
}
