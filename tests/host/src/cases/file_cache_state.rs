// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Audit both indexes and compare eviction choices with the original scan.

use alloc::vec::Vec;
use core::num::NonZeroU64;

use super::super::{
    CacheKey, ContentRevision, FileIdentity, FilePageIndex, FilesystemGeneration, NodeIdentity,
};
use super::{Access, Error, LoadToken, Slot, State};

fn key(page: u64) -> CacheKey {
    CacheKey::new(
        FileIdentity::new(
            FilesystemGeneration::new(NonZeroU64::MIN),
            NodeIdentity::new(NonZeroU64::MIN),
            ContentRevision::new(NonZeroU64::MIN),
        ),
        FilePageIndex::new(page),
    )
}

fn state(capacity: usize) -> State {
    State::try_new(capacity).unwrap_or_else(|error| panic!("cache construction: {error:?}"))
}

fn reserve(state: &mut State, key: CacheKey) -> LoadToken {
    match state.access(key) {
        Ok(Access::Reserved { token, .. }) => token,
        other => panic!("expected load reservation: {other:?}"),
    }
}

fn audit(state: &State) {
    let counts = state.snapshot();
    assert_eq!(state.clean(), counts.clean);
    assert_eq!(state.loading(), counts.loading);
    state.index.assert_valid(|slot| state.slots[slot].key());
    state.eviction.assert_valid(|slot| match state.slots[slot] {
        Slot::Clean { last_access, .. } => Some(last_access),
        Slot::Vacant | Slot::Loading { .. } => None,
    });
    assert_eq!(state.eviction.oldest(), oldest_by_scan(state));
    for (slot, entry) in state.slots.iter().enumerate() {
        if let Some(key) = entry.key() {
            assert_eq!(
                state
                    .index
                    .find(key, |index| state.slots[index].key() == Some(key)),
                Some(slot),
            );
        }
    }
}

fn oldest_by_scan(state: &State) -> Option<usize> {
    state
        .slots
        .iter()
        .enumerate()
        .filter_map(|(slot, entry)| match entry {
            Slot::Clean { last_access, .. } => Some((slot, *last_access)),
            Slot::Vacant | Slot::Loading { .. } => None,
        })
        .min_by_key(|(_, last_access)| *last_access)
        .map(|(slot, _)| slot)
}

fn collisions(capacity: usize, count: usize) -> Vec<CacheKey> {
    let mask = (capacity * 2).next_power_of_two() - 1;
    let keys: Vec<_> = (0..65536)
        .map(key)
        .filter(|key| key.bucket(mask) == 0)
        .take(count)
        .collect();
    assert_eq!(keys.len(), count);
    keys
}

#[test]
fn index_capacity_arithmetic_fails_before_allocation() {
    assert!(matches!(State::try_new(0), Err(Error::InvalidCapacity)));
    assert!(matches!(
        State::try_new(usize::MAX),
        Err(Error::InvalidCapacity)
    ));
}

#[test]
fn colliding_loading_keys_survive_head_middle_and_tail_abort() {
    let mut state = state(5);
    let keys = collisions(5, 5);
    let tokens: Vec<_> = keys.iter().map(|&key| reserve(&mut state, key)).collect();
    audit(&state);
    for &key in &keys {
        assert_eq!(state.access(key), Ok(Access::LoadInProgress));
    }
    // Insertions prepend, so the final token is the head and the first tail.
    for index in [2, 4, 0] {
        assert_eq!(state.abort(tokens[index]), Ok(()));
        assert_eq!(state.lookup(keys[index]), Ok(None));
        audit(&state);
    }
    for index in [1, 3] {
        assert_eq!(state.publish(tokens[index]), Ok(()));
        assert_eq!(state.lookup(keys[index]), Ok(Some(tokens[index].slot())));
        audit(&state);
    }
    for index in [0, 2, 4] {
        let token = reserve(&mut state, keys[index]);
        assert_eq!(state.publish(token), Ok(()));
        audit(&state);
    }
}

#[test]
fn collision_saturation_bypasses_before_reservation_or_eviction() {
    for capacity in [64, 128] {
        for publish in [false, true] {
            let mut state = state(capacity);
            let keys = collisions(capacity, super::index::MAX_CHAIN + 1);
            let tokens: Vec<_> = keys[..super::index::MAX_CHAIN]
                .iter()
                .map(|&key| {
                    let token = reserve(&mut state, key);
                    if publish {
                        assert_eq!(state.publish(token), Ok(()));
                    }
                    token
                })
                .collect();
            let slots = state.slots.clone();
            let index = state.index.snapshot();
            let eviction = state.eviction.snapshot();
            let sequences = (state.next_load_generation, state.next_access_sequence);
            assert_eq!(
                state.access(keys[super::index::MAX_CHAIN]),
                Ok(Access::CapacityBusy)
            );
            assert_eq!(state.slots, slots);
            assert_eq!(state.index.snapshot(), index);
            assert_eq!(state.eviction.snapshot(), eviction);
            assert_eq!(
                (state.next_load_generation, state.next_access_sequence),
                sequences
            );
            audit(&state);

            if publish {
                state.remove_clean(tokens[0].slot());
            } else {
                assert_eq!(state.abort(tokens[0]), Ok(()));
            }
            let retry = reserve(&mut state, keys[super::index::MAX_CHAIN]);
            assert_eq!(state.publish(retry), Ok(()));
            audit(&state);
        }
    }
}

#[test]
fn eviction_unlinks_old_key_and_stale_tokens_cannot_remove_replacement() {
    let mut state = state(2);
    let keys = collisions(2, 3);
    let first = reserve(&mut state, keys[0]);
    assert_eq!(state.publish(first), Ok(()));
    let second = reserve(&mut state, keys[1]);
    assert_eq!(state.publish(second), Ok(()));
    assert_eq!(state.lookup(keys[0]), Ok(Some(first.slot())));
    let replacement = reserve(&mut state, keys[2]);
    assert_eq!(replacement.slot(), second.slot());
    let slots = state.slots.clone();
    let index = state.index.snapshot();
    let eviction = state.eviction.snapshot();
    assert_eq!(state.abort(second), Err(Error::StaleLoad));
    assert_eq!(state.publish(second), Err(Error::StaleLoad));
    assert_eq!(state.slots, slots);
    assert_eq!(state.index.snapshot(), index);
    assert_eq!(state.eviction.snapshot(), eviction);
    assert_eq!(state.lookup(keys[1]), Ok(None));
    assert_eq!(state.access(keys[2]), Ok(Access::LoadInProgress));
    audit(&state);
    assert_eq!(state.publish(replacement), Ok(()));
    audit(&state);
}

#[test]
fn capacity_one_invalidation_reuses_slot_without_duplicate_free_entries() {
    let mut state = state(1);
    let stale = reserve(&mut state, key(1));
    state.invalidate_for_test(stale.slot());
    audit(&state);
    let replacement = reserve(&mut state, key(1));
    assert_eq!(replacement.slot(), stale.slot());
    assert_eq!(state.publish(stale), Err(Error::StaleLoad));
    assert_eq!(state.abort(stale), Err(Error::StaleLoad));
    audit(&state);
    assert_eq!(state.abort(replacement), Ok(()));
    assert_eq!(state.abort(replacement), Err(Error::StaleLoad));
    state.invalidate_for_test(replacement.slot());
    audit(&state);
}

#[test]
fn sequence_exhaustion_preserves_slots_indexes_and_free_stack() {
    for capacity in [1, 2] {
        for load_sequence_exhausted in [false, true] {
            let mut state = state(capacity);
            let existing = reserve(&mut state, key(1));
            assert_eq!(state.publish(existing), Ok(()));
            if load_sequence_exhausted {
                state.next_load_generation = u64::MAX;
            } else {
                state.next_access_sequence = u64::MAX;
            }
            let slots = state.slots.clone();
            let index = state.index.snapshot();
            let eviction = state.eviction.snapshot();
            assert_eq!(state.access(key(2)), Err(Error::SequenceExhausted));
            assert_eq!(state.slots, slots);
            assert_eq!(state.index.snapshot(), index);
            assert_eq!(state.eviction.snapshot(), eviction);
            audit(&state);
        }
    }
}

#[test]
fn exhausted_hit_preserves_recency_and_pending_publication_needs_no_sequence() {
    let mut state = state(2);
    let older = reserve(&mut state, key(1));
    let newer = reserve(&mut state, key(2));
    assert_eq!(state.publish(newer), Ok(()));
    state.exhaust_sequences_for_test();
    let slots = state.slots.clone();
    let index = state.index.snapshot();
    let eviction = state.eviction.snapshot();
    assert_eq!(state.lookup(key(2)), Err(Error::SequenceExhausted));
    assert_eq!(state.slots, slots);
    assert_eq!(state.index.snapshot(), index);
    assert_eq!(state.eviction.snapshot(), eviction);
    assert_eq!(state.publish(older), Ok(()));
    assert_eq!(state.eviction.oldest(), Some(older.slot()));
    audit(&state);
}

#[test]
fn vacant_slots_take_priority_over_clean_victims() {
    let mut state = state(3);
    let first = reserve(&mut state, key(1));
    let second = reserve(&mut state, key(2));
    assert_eq!(state.publish(first), Ok(()));
    assert_eq!(state.publish(second), Ok(()));
    let vacant = state.index.vacant();
    let third = reserve(&mut state, key(3));
    assert_eq!(Some(third.slot()), vacant);
    assert_eq!(state.snapshot().evictions, 0);
    assert_eq!(state.abort(third), Ok(()));
    let fourth = reserve(&mut state, key(4));
    assert_eq!(fourth.slot(), third.slot());
    assert_eq!(state.snapshot().evictions, 0);
    assert_eq!(state.eviction.oldest(), Some(first.slot()));
    audit(&state);
}

#[test]
fn eviction_still_uses_original_access_sequence_not_publication_order() {
    let mut state = state(3);
    let older = reserve(&mut state, key(1));
    let newer = reserve(&mut state, key(2));
    assert_eq!(state.publish(newer), Ok(()));
    assert_eq!(state.lookup(key(2)), Ok(Some(newer.slot())));
    assert_eq!(state.publish(older), Ok(()));
    let third = reserve(&mut state, key(3));
    assert_eq!(state.publish(third), Ok(()));
    let fourth = reserve(&mut state, key(4));
    assert_eq!(fourth.slot(), older.slot());
    assert_eq!(state.lookup(key(1)), Ok(None));
    audit(&state);
}

#[test]
fn reverse_publication_and_hits_keep_the_same_scan_order() {
    let mut state = state(7);
    let tokens: Vec<_> = (0..7).map(|page| reserve(&mut state, key(page))).collect();
    for token in tokens.iter().rev() {
        assert_eq!(state.publish(*token), Ok(()));
        audit(&state);
    }
    for page in [0, 4, 1, 6] {
        assert_eq!(
            state.lookup(key(page)),
            Ok(Some(tokens[page as usize].slot()))
        );
        audit(&state);
    }
    for page in 7..14 {
        let expected = oldest_by_scan(&state);
        let next = reserve(&mut state, key(page));
        assert_eq!(Some(next.slot()), expected);
        assert_eq!(state.publish(next), Ok(()));
        audit(&state);
    }
}

#[test]
fn mixed_transitions_keep_every_slot_in_exactly_one_owner_set() {
    for capacity in [1, 3, 16] {
        let mut state = state(capacity);
        let mut pending = Vec::new();
        let mut random = 1_u64;
        for _ in 0..2000 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let selected = key((random >> 16) % 47);
            match (random >> 8) % 5 {
                0 => {
                    let expected = state.index.vacant().or_else(|| oldest_by_scan(&state));
                    if let Ok(Access::Reserved { token, .. }) = state.access(selected) {
                        assert_eq!(Some(token.slot()), expected);
                        pending.push(token);
                    }
                }
                1 => {
                    if let Some(token) = pending.pop() {
                        assert!(matches!(
                            state.publish(token),
                            Ok(()) | Err(Error::StaleLoad)
                        ));
                    }
                }
                2 => {
                    if let Some(token) = pending.pop() {
                        assert!(matches!(state.abort(token), Ok(()) | Err(Error::StaleLoad)));
                    }
                }
                3 => state.invalidate_for_test((random >> 32) as usize % capacity),
                _ => {
                    let _ = state.lookup(selected);
                }
            }
            audit(&state);
        }
    }
}
