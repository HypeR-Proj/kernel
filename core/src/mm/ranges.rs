// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// A half-open physical address interval `[start, start + size)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalRange {
    start: u64,
    size: u64,
}

impl PhysicalRange {
    pub const EMPTY: Self = Self { start: 0, size: 0 };

    pub const fn new(start: u64, size: u64) -> Option<Self> {
        if size == 0 || start.checked_add(size).is_none() {
            None
        } else {
            Some(Self { start, size })
        }
    }

    pub const fn end(self) -> u64 {
        // Construction guarantees that this addition cannot overflow.
        self.start + self.size
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn size(self) -> u64 {
        self.size
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end() && other.start < self.end()
    }
}

/// Allocation-free region storage used before the heap exists.
#[derive(Clone, Copy, Debug)]
pub struct RegionList<const CAPACITY: usize> {
    entries: [PhysicalRange; CAPACITY],
    length: usize,
}

impl<const CAPACITY: usize> RegionList<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            entries: [PhysicalRange::EMPTY; CAPACITY],
            length: 0,
        }
    }

    pub fn as_slice(&self) -> &[PhysicalRange] {
        &self.entries[..self.length]
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    /// Inserts a range and coalesces overlapping or adjacent entries.
    pub fn insert(&mut self, mut range: PhysicalRange) -> Result<(), CapacityError> {
        if range.size == 0 || range.start.checked_add(range.size).is_none() {
            return Err(CapacityError);
        }
        let mut index = 0;
        while index < self.length {
            let current = self.entries[index];
            if range.start <= current.end() && current.start <= range.end() {
                let start = range.start.min(current.start);
                let end = range.end().max(current.end());
                range = PhysicalRange::new(start, end - start).ok_or(CapacityError)?;
                self.remove(index);
            } else {
                index += 1;
            }
        }

        if self.length == CAPACITY {
            return Err(CapacityError);
        }
        self.entries[self.length] = range;
        self.length += 1;
        self.entries[..self.length].sort_unstable_by_key(|entry| entry.start);
        Ok(())
    }

    fn remove(&mut self, index: usize) {
        self.entries.copy_within(index + 1..self.length, index);
        self.length -= 1;
        self.entries[self.length] = PhysicalRange::EMPTY;
    }
}

impl<const CAPACITY: usize> Default for RegionList<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityError;
