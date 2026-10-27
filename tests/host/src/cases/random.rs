// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! PRNG reproducibility, boundary behavior and firmware failure contracts.

use core::num::NonZeroU64;
use {hyper::random::Random, hyper_drivers::random::trng};

#[test]
fn splitmix64_reference_sequence_and_state_size() {
    assert_eq!(core::mem::size_of::<Random>(), 8);
    let mut random = Random::new(0);
    for expected in [
        0xe220_a839_7b1d_cdaf,
        0x6e78_9e6a_a1b9_65f4,
        0x06c4_5d18_8009_454f,
        0xf88b_b8a8_724c_81ec,
    ] {
        assert_eq!(random.next_u64(), expected);
    }
}

#[test]
fn arbitrary_buffer_lengths_do_not_overwrite_guards_or_consume_on_empty() {
    for length in 0..=257 {
        let mut random = Random::new(u64::MAX);
        let mut reference = Random::new(u64::MAX);
        let mut storage = [0xa5; 259];
        random.fill_bytes(&mut storage[1..1 + length]);
        assert_eq!(storage[0], 0xa5);
        assert_eq!(storage[length + 1], 0xa5);
        for chunk in storage[1..1 + length].chunks(8) {
            assert_eq!(chunk, &reference.next_u64().to_le_bytes()[..chunk.len()]);
        }
        assert_eq!(random.next_u64(), reference.next_u64());
    }
}

#[test]
fn bounded_values_cover_extreme_ranges_without_overflow() {
    let mut random = Random::new(u64::MAX);
    for upper in [1, 2, 3, 257, (1 << 63) + 1, u64::MAX] {
        let bound = crate::require_some(NonZeroU64::new(upper));
        for _ in 0..1000 {
            assert!(random.below(bound) < upper);
        }
    }
}

#[test]
fn stream_advances_without_new_entropy_and_mixing_changes_it() {
    let mut first = Random::new(42);
    let mut second = Random::new(42);
    let before = first.next_u64();
    assert_eq!(before, second.next_u64());
    let after = first.next_u64();
    assert_ne!(before, after);
    second.mix(0x1234_5678);
    assert_ne!(after, second.next_u64());
}

#[test]
fn firmware_uses_x3_for_64_bit_sample_and_preserves_zero() {
    for sample in [0, 0x1234_5678_90ab_cdef] {
        let mut calls = Vec::new();
        assert_eq!(
            trng::seed(|function, argument| {
                calls.push((function, argument));
                match function {
                    0x8400_0050 => [0x0001_0000, 0, 0, 0],
                    0x8400_0051 => [0, 0, 0, 0],
                    0xc400_0053 => [0, 0xdead, 0xbeef, sample],
                    _ => panic!("unexpected TRNG function"),
                }
            }),
            Some(sample)
        );
        assert_eq!(
            calls,
            [
                (0x8400_0050, 0),
                (0x8400_0051, 0xc400_0053),
                (0xc400_0053, 64)
            ]
        );
    }
}

#[test]
fn firmware_failure_never_returns_uninitialized_registers_or_polls() {
    for failure_at in 0..3 {
        for status in [u64::MAX, u64::MAX - 2, 0xffff_fffc] {
            let mut calls = 0;
            let result = trng::seed(|_, _| {
                let current = calls;
                calls += 1;
                if current == failure_at {
                    [status, 1, 2, 3]
                } else if current == 0 {
                    [0x10000, 0, 0, 0]
                } else {
                    [0, 0, 0, 0]
                }
            });
            assert_eq!(result, None);
            assert_eq!(calls, failure_at + 1);
        }
    }
}
