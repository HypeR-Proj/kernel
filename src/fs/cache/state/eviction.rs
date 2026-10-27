// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Preallocated minimum heap of clean slots, ordered by their last access.

use alloc::vec::Vec;
use core::alloc::Layout;

use super::Error;

const NONE: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Entry {
    slot: usize,
    last_access: u64,
}

impl Entry {
    fn older_than(self, other: Self) -> bool {
        (self.last_access, self.slot) < (other.last_access, other.slot)
    }
}

pub(super) struct EvictionHeap {
    entries: Vec<Entry>,
    positions: Vec<usize>,
}

impl EvictionHeap {
    pub(super) fn allocation_layouts(capacity: usize) -> Result<[Layout; 2], Error> {
        if capacity == 0 {
            return Err(Error::InvalidCapacity);
        }
        Ok([
            Layout::array::<Entry>(capacity).map_err(|_| Error::InvalidCapacity)?,
            Layout::array::<usize>(capacity).map_err(|_| Error::InvalidCapacity)?,
        ])
    }

    pub(super) fn try_new(capacity: usize) -> Result<Self, Error> {
        // Check both array layouts before allocating either one. The positions
        // array also records the configured limit, independent of Vec capacity.
        Self::allocation_layouts(capacity)?;
        let entries = super::super::memory::vector(capacity)?;
        let mut positions = super::super::memory::vector(capacity)?;
        positions.resize(capacity, NONE);
        Ok(Self { entries, positions })
    }

    pub(super) fn oldest(&self) -> Option<usize> {
        self.entries.first().map(|entry| entry.slot)
    }

    pub(super) fn candidate(&self, index: usize) -> Option<usize> {
        self.entries.get(index).map(|entry| entry.slot)
    }

    pub(super) fn insert(&mut self, slot: usize, last_access: u64) {
        if self.positions.get(slot) != Some(&NONE) || self.entries.len() >= self.positions.len() {
            invariant_failure();
        }
        let position = self.entries.len();
        self.set_position(slot, position);
        // Construction reserves the complete configured capacity. The bound
        // above prevents this push from allocating while the cache is locked.
        self.entries.push(Entry { slot, last_access });
        self.sift_up(position);
    }

    pub(super) fn touch(&mut self, slot: usize, last_access: u64) {
        let position = self.position(slot);
        if last_access <= self.entries[position].last_access {
            invariant_failure();
        }
        self.entries[position].last_access = last_access;
        // Hits reserve strictly increasing sequences, so only descendants can
        // precede the updated entry. Publication instead uses insert because
        // a delayed load retains its older reservation-time sequence.
        self.sift_down(position);
    }

    pub(super) fn remove(&mut self, slot: usize) {
        let position = self.position(slot);
        let Some(last) = self.entries.pop() else {
            invariant_failure();
        };
        self.set_position(slot, NONE);
        if position == self.entries.len() {
            return;
        }
        self.entries[position] = last;
        self.set_position(last.slot, position);
        // Test invalidation can remove an arbitrary clean slot, not just the
        // oldest root. Its replacement may need to move in either direction.
        if position != 0 && last.older_than(self.entries[(position - 1) / 2]) {
            self.sift_up(position);
        } else {
            self.sift_down(position);
        }
    }

    fn position(&self, slot: usize) -> usize {
        let Some(&position) = self.positions.get(slot) else {
            invariant_failure();
        };
        if self.entries.get(position).map(|entry| entry.slot) != Some(slot) {
            invariant_failure();
        }
        position
    }

    fn set_position(&mut self, slot: usize, position: usize) {
        let Some(entry) = self.positions.get_mut(slot) else {
            invariant_failure();
        };
        *entry = position;
    }

    fn swap(&mut self, first: usize, second: usize) {
        self.entries.swap(first, second);
        self.set_position(self.entries[first].slot, first);
        self.set_position(self.entries[second].slot, second);
    }

    fn sift_up(&mut self, mut position: usize) {
        while position != 0 {
            let parent = (position - 1) / 2;
            if !self.entries[position].older_than(self.entries[parent]) {
                break;
            }
            self.swap(position, parent);
            position = parent;
        }
    }

    fn sift_down(&mut self, mut position: usize) {
        loop {
            // The checked Entry array layout bounds this arithmetic well
            // below usize::MAX for every valid heap position.
            let left = position * 2 + 1;
            if left >= self.entries.len() {
                break;
            }
            let right = left + 1;
            let oldest = if right < self.entries.len()
                && self.entries[right].older_than(self.entries[left])
            {
                right
            } else {
                left
            };
            if !self.entries[oldest].older_than(self.entries[position]) {
                break;
            }
            self.swap(position, oldest);
            position = oldest;
        }
    }

    #[cfg(test)]
    pub(super) fn assert_valid(&self, last_access: impl Fn(usize) -> Option<u64>) {
        assert!(self.entries.len() <= self.positions.len());
        for (position, entry) in self.entries.iter().enumerate() {
            assert_eq!(self.positions[entry.slot], position);
            assert_eq!(last_access(entry.slot), Some(entry.last_access));
            if position != 0 {
                assert!(!entry.older_than(self.entries[(position - 1) / 2]));
            }
        }
        for (slot, &position) in self.positions.iter().enumerate() {
            match last_access(slot) {
                Some(sequence) => {
                    assert!(position < self.entries.len());
                    assert_eq!(self.entries[position].slot, slot);
                    assert_eq!(self.entries[position].last_access, sequence);
                }
                None => assert_eq!(position, NONE),
            }
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (Vec<(usize, u64)>, Vec<usize>) {
        (
            self.entries
                .iter()
                .map(|entry| (entry.slot, entry.last_access))
                .collect(),
            self.positions.clone(),
        )
    }
}

#[cold]
fn invariant_failure() -> ! {
    hyper::debug::invariant_failure("I/O cache eviction heap invariant")
}

#[cfg(test)]
#[path = "../../../../tests/host/src/cases/file_cache_eviction.rs"]
mod tests;
