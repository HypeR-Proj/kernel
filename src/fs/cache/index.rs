// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Preallocated key chains and vacant slots; payloads stay in the cache owner.

use alloc::vec::Vec;
use core::alloc::Layout;

use super::CacheKey;
use super::state::Error;

const NONE: usize = usize::MAX;
pub(super) const MAX_CHAIN: usize = 64;

pub(super) struct Index {
    heads: Vec<usize>,
    next: Vec<usize>,
    free: Vec<usize>,
}

impl Index {
    pub(super) fn allocation_layouts(capacity: usize) -> Result<[Layout; 3], Error> {
        let buckets = bucket_count(capacity)?;
        let heads = Layout::array::<usize>(buckets).map_err(|_| Error::InvalidCapacity)?;
        let slots = Layout::array::<usize>(capacity).map_err(|_| Error::InvalidCapacity)?;
        Ok([heads, slots, slots])
    }

    pub(super) fn try_new(capacity: usize) -> Result<Self, Error> {
        let buckets = bucket_count(capacity)?;
        let mut heads = super::memory::vector(buckets)?;
        heads.resize(buckets, NONE);
        let mut next = super::memory::vector(capacity)?;
        next.resize(capacity, NONE);
        let mut free = super::memory::vector(capacity)?;
        free.extend((0..capacity).rev());
        Ok(Self { heads, next, free })
    }

    pub(super) fn vacant(&self) -> Option<usize> {
        self.free.last().copied()
    }

    pub(super) fn take_vacant(&mut self, slot: usize) {
        if self.free.pop() != Some(slot) {
            invariant_failure();
        }
    }

    pub(super) fn release(&mut self, slot: usize) {
        if slot >= self.next.len() || self.next[slot] != NONE || self.free.len() == self.next.len()
        {
            invariant_failure();
        }
        // Every release follows a validated occupied -> vacant transition;
        // the stack was allocated to the complete slot capacity at construction.
        self.free.push(slot);
    }

    pub(super) fn find(&self, key: CacheKey, matches: impl Fn(usize) -> bool) -> Option<usize> {
        let mut slot = self.heads[key.bucket(self.heads.len() - 1)];
        for _ in 0..MAX_CHAIN {
            if slot == NONE {
                return None;
            }
            if slot >= self.next.len() {
                invariant_failure();
            }
            if matches(slot) {
                return Some(slot);
            }
            slot = self.next[slot];
        }
        if slot != NONE {
            invariant_failure();
        }
        None
    }

    /// Collision saturation skips optional admission instead of extending an
    /// IRQ-masked lookup in proportion to the total cache capacity.
    pub(super) fn can_insert(&self, key: CacheKey) -> bool {
        let mut slot = self.heads[key.bucket(self.heads.len() - 1)];
        for _ in 0..MAX_CHAIN {
            if slot == NONE {
                return true;
            }
            let Some(&next) = self.next.get(slot) else {
                invariant_failure();
            };
            slot = next;
        }
        if slot != NONE {
            invariant_failure();
        }
        false
    }

    pub(super) fn insert(&mut self, key: CacheKey, slot: usize) {
        let bucket = key.bucket(self.heads.len() - 1);
        if slot >= self.next.len()
            || self.next[slot] != NONE
            || self.heads[bucket] == slot
            || !self.can_insert(key)
        {
            invariant_failure();
        }
        self.next[slot] = self.heads[bucket];
        self.heads[bucket] = slot;
    }

    pub(super) fn remove(&mut self, key: CacheKey, target: usize) {
        let bucket = key.bucket(self.heads.len() - 1);
        let mut slot = self.heads[bucket];
        let mut previous = NONE;
        for _ in 0..MAX_CHAIN {
            if slot >= self.next.len() {
                invariant_failure();
            }
            let next = self.next[slot];
            if slot == target {
                if previous == NONE {
                    self.heads[bucket] = next;
                } else {
                    self.next[previous] = next;
                }
                self.next[slot] = NONE;
                return;
            }
            previous = slot;
            slot = next;
        }
        invariant_failure();
    }

    #[cfg(test)]
    pub(super) fn assert_valid(&self, key: impl Fn(usize) -> Option<CacheKey>) {
        let mut seen = alloc::vec![false; self.next.len()];
        for (bucket, &head) in self.heads.iter().enumerate() {
            let mut slot = head;
            let mut chain = 0;
            while slot != NONE {
                chain += 1;
                assert!(
                    chain <= MAX_CHAIN,
                    "collision chain exceeds lock-work bound"
                );
                assert!(slot < self.next.len());
                assert!(!seen[slot], "duplicate or cyclic index link");
                seen[slot] = true;
                let key = key(slot).unwrap_or_else(|| panic!("indexed vacant slot"));
                assert_eq!(key.bucket(self.heads.len() - 1), bucket);
                slot = self.next[slot];
            }
        }
        for &slot in &self.free {
            assert!(slot < self.next.len());
            assert!(
                !seen[slot],
                "duplicate free slot or occupied slot on free stack"
            );
            seen[slot] = true;
            assert!(key(slot).is_none());
            assert_eq!(self.next[slot], NONE);
        }
        assert!(seen.into_iter().all(|seen| seen), "unreachable slot");
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
        (self.heads.clone(), self.next.clone(), self.free.clone())
    }
}

fn bucket_count(capacity: usize) -> Result<usize, Error> {
    capacity
        .checked_mul(2)
        .and_then(usize::checked_next_power_of_two)
        .filter(|_| capacity != 0)
        .ok_or(Error::InvalidCapacity)
}

#[cold]
pub(super) fn invariant_failure() -> ! {
    hyper::debug::invariant_failure("I/O cache index invariant")
}
