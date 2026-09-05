// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible heap-allocation behavior exposed by the public memory API.

#[test]
fn fallible_box_supports_values_and_zero_sized_types() {
    let value = crate::require_ok(hyper::mm::try_box(0x4859_5045_u32));
    assert_eq!(*value, 0x4859_5045);

    let zero_sized = crate::require_ok(hyper::mm::try_box(()));
    assert_eq!(*zero_sized, ());
}
