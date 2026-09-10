// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! UTC representation and arithmetic independently checked in wider integers.

use hyper::time::Timestamp;

#[test]
fn negative_utc_uses_floor_seconds_and_nonnegative_nanoseconds() {
    let before = crate::require_some(Timestamp::new(-1, 750_000_000));
    let epoch = crate::require_some(Timestamp::new(0, 0));
    assert!(before < epoch);
    assert_eq!(before.checked_add_nanoseconds(250_000_000), Some(epoch));
    assert_eq!(before.seconds(), -1);
    assert_eq!(before.nanoseconds(), 750_000_000);
    assert_eq!(Timestamp::new(0, 1_000_000_000), None);
    assert_eq!(Timestamp::new(-1, u32::MAX), None);
}

#[test]
fn timestamp_carry_and_overflow_match_wide_integer_arithmetic() {
    for seconds in [i64::MIN, -1, 0, i64::MAX - 20_000_000_000, i64::MAX] {
        for nanoseconds in [0, 17, 999_999_999] {
            let timestamp = crate::require_some(Timestamp::new(seconds, nanoseconds));
            for delta in [0, 1, 999_999_999, 1_000_000_000, u64::MAX] {
                let total = i128::from(seconds) * 1_000_000_000
                    + i128::from(nanoseconds)
                    + i128::from(delta);
                let expected = i64::try_from(total.div_euclid(1_000_000_000))
                    .ok()
                    .and_then(|seconds| {
                        let nanos = u32::try_from(total.rem_euclid(1_000_000_000)).ok()?;
                        Timestamp::new(seconds, nanos)
                    });
                assert_eq!(timestamp.checked_add_nanoseconds(delta), expected);
            }
        }
    }
}
