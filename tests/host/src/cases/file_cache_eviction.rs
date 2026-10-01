// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Heap ordering and inverse positions across arbitrary clean-slot removal.

use alloc::vec;
use alloc::vec::Vec;

use super::{Error, EvictionHeap};

fn heap(capacity: usize) -> EvictionHeap {
    EvictionHeap::try_new(capacity)
        .unwrap_or_else(|error| panic!("eviction heap construction: {error:?}"))
}

fn assert_order(heap: &EvictionHeap, sequences: &[Option<u64>]) {
    heap.assert_valid(|slot| sequences[slot]);
    let oldest = sequences
        .iter()
        .enumerate()
        .filter_map(|(slot, sequence)| sequence.map(|sequence| (sequence, slot)))
        .min()
        .map(|(_, slot)| slot);
    assert_eq!(heap.oldest(), oldest);
}

fn drain(heap: &mut EvictionHeap, sequences: &mut [Option<u64>]) {
    assert_order(heap, sequences);
    while let Some(slot) = heap.oldest() {
        heap.remove(slot);
        sequences[slot] = None;
        assert_order(heap, sequences);
    }
}

#[test]
fn invalid_array_layouts_are_rejected_before_allocation() {
    assert!(matches!(
        EvictionHeap::try_new(0),
        Err(Error::InvalidCapacity)
    ));
    assert!(matches!(
        EvictionHeap::try_new(usize::MAX),
        Err(Error::InvalidCapacity)
    ));
}

#[test]
fn root_middle_and_last_removal_preserve_inverse_positions() {
    for removed in [0, 3, 6] {
        let mut heap = heap(7);
        let mut sequences: Vec<_> = [1, 5, 2, 6, 7, 3, 4].into_iter().map(Some).collect();
        for (slot, sequence) in sequences.iter().copied().enumerate() {
            heap.insert(slot, sequence.unwrap_or_else(|| panic!("missing sequence")));
        }
        heap.remove(removed);
        sequences[removed] = None;
        if removed == 3 {
            // The last entry (sequence 4) replaces sequence 6, then rises past
            // its parent (sequence 5). Root-only removal would miss this case.
            assert_eq!(heap.snapshot().1[6], 1);
        }
        drain(&mut heap, &mut sequences);
    }
}

#[test]
fn removing_a_middle_entry_can_require_sifting_down() {
    let mut heap = heap(9);
    let mut sequences: Vec<_> = (1..=9).map(Some).collect();
    for slot in 0..9 {
        heap.insert(slot, slot as u64 + 1);
    }
    heap.remove(1);
    sequences[1] = None;
    assert_eq!(heap.snapshot().1[8], 7);
    drain(&mut heap, &mut sequences);
}

#[test]
fn hits_move_older_entries_down_and_delayed_insertions_move_up() {
    let mut heap = heap(4);
    let mut sequences = vec![None; 4];
    for (slot, sequence) in [(3, 4), (2, 3), (1, 2), (0, 1)] {
        heap.insert(slot, sequence);
        sequences[slot] = Some(sequence);
        assert_order(&heap, &sequences);
    }
    for (slot, sequence) in [(0, 5), (2, 6), (1, 7), (3, 8)] {
        heap.touch(slot, sequence);
        sequences[slot] = Some(sequence);
        assert_order(&heap, &sequences);
    }
    drain(&mut heap, &mut sequences);
}

#[test]
fn equal_sequences_keep_the_original_slot_order_tiebreak() {
    let mut heap = heap(4);
    let mut sequences = vec![Some(1); 4];
    for slot in (0..4).rev() {
        heap.insert(slot, 1);
    }
    for expected in 0..4 {
        assert_eq!(heap.oldest(), Some(expected));
        heap.remove(expected);
        sequences[expected] = None;
        assert_order(&heap, &sequences);
    }
}

#[test]
fn capacity_one_can_be_removed_and_reinserted_without_growing() {
    let mut heap = heap(1);
    let allocation = heap.entries.as_ptr();
    for sequence in 1..100 {
        heap.insert(0, sequence);
        assert_order(&heap, &[Some(sequence)]);
        assert_eq!(heap.entries.as_ptr(), allocation);
        heap.remove(0);
        assert_order(&heap, &[None]);
    }
}
