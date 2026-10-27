// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Only handle-internal tests depend on the private slot/generation layout.

use super::{
    GENERATION_LIMIT, HANDLE_PREFIX, HANDLE_PREFIX_MASK, HandleCodec, HandleError, MAX_SLOTS,
    PAYLOAD_MASK, SLOT_MASK, wire,
};
use super::{HandleTable, Slot};

#[test]
fn every_codec_keeps_valid_values_nonzero_at_encoding_boundaries() {
    assert_eq!(HANDLE_PREFIX, 0x0f00_0000_0000_0000);
    assert_eq!(HANDLE_PREFIX_MASK, 0xff00_0000_0000_0000);
    for seed in [
        0,
        1,
        2,
        SLOT_MASK,
        SLOT_MASK + 1,
        PAYLOAD_MASK,
        HANDLE_PREFIX,
        HANDLE_PREFIX_MASK,
        u64::MAX,
    ] {
        let codec = HandleCodec::new(seed);
        assert_ne!(codec.mask, 0);
        assert_eq!(codec.mask & HANDLE_PREFIX_MASK, 0);
        for slot in [0, 1, MAX_SLOTS - 1] {
            for generation in [1, 2, GENERATION_LIMIT] {
                let value = codec.encode(slot, generation);
                assert_ne!(wire::to_abi(value), 0);
                assert_eq!(wire::to_abi(value) & HANDLE_PREFIX_MASK, HANDLE_PREFIX);
                assert_eq!(codec.decode(value), Ok((slot, generation)));
            }
        }
    }
}

#[test]
fn fixed_reservation_rollback_wraps_without_consuming_slot_capacity() {
    let mut table = HandleTable::new(99);
    let mut reservation = crate::require_ok(table.reserve::<1>());
    let index = reservation.slots[0];
    reservation.generations[0] = GENERATION_LIMIT;
    table.slots.replace(
        index,
        Slot::Reserved {
            generation: GENERATION_LIMIT,
            reservation: reservation.reservation,
        },
    );
    let page_count = table.slots.len();
    reservation.abort(&mut table);
    let replacement = crate::require_ok(table.reserve::<1>());
    assert_eq!(table.codec.decode(replacement.values()[0]), Ok((index, 1)));
    assert_eq!(table.slots.len(), page_count);
    replacement.abort(&mut table);
    assert!(table.free_list_is_consistent_for_test());
}

#[test]
fn batch_trim_and_rollback_recycle_wrapped_slots() {
    let mut table = HandleTable::new(123);
    let mut reservation = crate::require_ok(table.reserve_batch(2));
    let indices = [reservation.slots[0].index, reservation.slots[1].index];
    for (slot, value) in reservation.slots.iter_mut().zip(&mut reservation.values) {
        slot.generation = GENERATION_LIMIT;
        *value = table.codec.encode(slot.index, GENERATION_LIMIT);
        table.slots.replace(
            slot.index,
            Slot::Reserved {
                generation: GENERATION_LIMIT,
                reservation: reservation.reservation,
            },
        );
    }
    let page_count = table.slots.len();
    reservation.trim_to(&mut table, 1);
    drop(reservation.abort(&mut table));
    let replacement = crate::require_ok(table.reserve_batch(2));
    for &value in replacement.values() {
        let (slot, generation) = crate::require_ok(table.codec.decode(value));
        assert!(indices.contains(&slot));
        assert_eq!(generation, 1);
    }
    assert_eq!(table.slots.len(), page_count);
    drop(replacement.abort(&mut table));
    assert!(table.free_list_is_consistent_for_test());
}

#[test]
fn zero_encoded_slot_bits_are_valid_only_after_unmasking() {
    let codec = HandleCodec::new(0x005a_93d7);
    let slot = codec.mask as usize - 1;
    let value = codec.encode(slot, 1);
    assert_eq!(wire::to_abi(value) & SLOT_MASK, 0);
    let parsed = match wire::from_abi(wire::to_abi(value)) {
        Ok(parsed) => parsed,
        Err(_) => panic!("nonzero masked token rejected at the ABI boundary"),
    };
    assert_eq!(codec.decode(parsed), Ok((slot, 1)));
}

#[test]
fn zero_masked_payload_still_names_the_original_slot() {
    let slot = 42;
    let generation = GENERATION_LIMIT;
    let codec = HandleCodec::new((generation << super::SLOT_BITS) | (slot as u64 + 1));
    let value = codec.encode(slot, generation);
    assert_eq!(wire::to_abi(value), HANDLE_PREFIX);
    assert_eq!(wire::from_abi(HANDLE_PREFIX), Ok(value));
    assert_eq!(codec.decode(value), Ok((slot, generation)));
}

#[test]
fn reserved_prefixes_cannot_alias_a_valid_handle() {
    let codec = HandleCodec::new(u64::MAX);
    let value = codec.encode(0, GENERATION_LIMIT);
    let payload = wire::to_abi(value) & PAYLOAD_MASK;
    assert_eq!(wire::from_abi(0), Err(HandleError::InvalidHandle));
    for prefix in (0..=255_u64)
        .map(|byte| byte << 56)
        .filter(|&prefix| prefix != HANDLE_PREFIX)
    {
        for bits in [0, payload, PAYLOAD_MASK] {
            assert_eq!(
                wire::from_abi(prefix | bits),
                Err(HandleError::InvalidHandle)
            );
        }
    }
    assert_eq!(wire::from_abi(wire::to_abi(value)), Ok(value));
}

#[test]
fn invalid_internal_fields_are_rejected_before_slot_subtraction() {
    let codec = HandleCodec::new(123);
    // Both wire tokens are nonzero; one decodes to slot zero, the other to
    // generation zero. Neither may underflow an index or enter a table lookup.
    for raw in [
        HANDLE_PREFIX | ((1 << super::SLOT_BITS) ^ codec.mask),
        HANDLE_PREFIX | (1 ^ codec.mask),
    ] {
        let value = match wire::from_abi(raw) {
            Ok(value) => value,
            Err(_) => panic!("test token must be nonzero"),
        };
        assert_eq!(codec.decode(value), Err(HandleError::InvalidHandle));
    }
}
