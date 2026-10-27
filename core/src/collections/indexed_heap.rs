// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free binary heap operations over caller-owned indexed storage.
//!
//! Adapters own capacity, insertion/removal and handle validity. `swap` must
//! update reverse positions as well as storage. Ordering and swaps must not
//! change length during an operation. All methods use static dispatch.

pub trait Storage {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Strict ordering; callers retain domain-specific tie and wrap rules.
    fn precedes(&self, left: usize, right: usize) -> bool;
    fn swap(&mut self, left: usize, right: usize);
}

/// Restore order after appending an entry; out-of-range positions are ignored.
pub fn sift_up<S: Storage>(storage: &mut S, mut position: usize) {
    if position >= storage.len() {
        return;
    }
    while position != 0 {
        let parent = (position - 1) / 2;
        if !storage.precedes(position, parent) {
            break;
        }
        storage.swap(position, parent);
        position = parent;
    }
}

/// Restore order after replacing/removing an entry or changing its rank.
/// The remaining entries must already form a heap. No allocation occurs.
pub fn repair<S: Storage>(storage: &mut S, mut position: usize) {
    let len = storage.len();
    if position >= len {
        return;
    }
    if position != 0 && storage.precedes(position, (position - 1) / 2) {
        sift_up(storage, position);
        return;
    }
    // Only internal nodes have children. This also bounds index arithmetic.
    while position < len / 2 {
        let left = position * 2 + 1;
        let right = left + 1;
        let child = if right < len && storage.precedes(right, left) {
            right
        } else {
            left
        };
        if !storage.precedes(child, position) {
            break;
        }
        storage.swap(position, child);
        position = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Heap {
        entries: std::vec::Vec<usize>,
        positions: [usize; 64],
        ranks: [u32; 64],
    }
    impl Storage for Heap {
        fn len(&self) -> usize {
            self.entries.len()
        }
        fn precedes(&self, l: usize, r: usize) -> bool {
            (self.ranks[self.entries[l]], self.entries[l])
                < (self.ranks[self.entries[r]], self.entries[r])
        }
        fn swap(&mut self, l: usize, r: usize) {
            self.entries.swap(l, r);
            self.positions[self.entries[l]] = l;
            self.positions[self.entries[r]] = r;
        }
    }
    fn verify(heap: &Heap) {
        for (pos, &id) in heap.entries.iter().enumerate() {
            assert_eq!(heap.positions[id], pos);
            if pos != 0 {
                assert!(!heap.precedes(pos, (pos - 1) / 2));
            }
        }
    }
    #[test]
    fn insert_reprioritize_and_remove_preserve_heap_and_reverse_positions() {
        let mut h = Heap {
            entries: std::vec::Vec::new(),
            positions: [0; 64],
            ranks: [0; 64],
        };
        for id in 0..64 {
            h.ranks[id] = ((id * 19) % 31) as u32;
            h.entries.push(id);
            h.positions[id] = id;
            sift_up(&mut h, id);
            verify(&h);
        }
        for id in 0..64 {
            h.ranks[id] = if id % 2 == 0 { 0 } else { 100 };
            let pos = h.positions[id];
            repair(&mut h, pos);
            verify(&h);
        }
        for id in (0..64).rev() {
            let pos = h.positions[id];
            let last = h.len() - 1;
            h.swap(pos, last);
            assert_eq!(h.entries.pop(), Some(id));
            repair(&mut h, pos);
            verify(&h);
        }
        repair(&mut h, usize::MAX);
        sift_up(&mut h, usize::MAX);
        assert!(h.is_empty());
    }
}
