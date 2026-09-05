// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reproducible KASLR offset selection and geometry validation.

use hyper::mm::kaslr::{self, Error};

#[test]
fn selects_a_reproducible_aligned_offset_inside_the_window() {
    let first = crate::require_ok(kaslr::select_offset(
        0x0123_4567_89ab_cdef,
        0x90000,
        512 * 1024 * 1024 * 1024,
        2 * 1024 * 1024,
    ));
    let second = crate::require_ok(kaslr::select_offset(
        0x0123_4567_89ab_cdef,
        0x90000,
        512 * 1024 * 1024 * 1024,
        2 * 1024 * 1024,
    ));

    assert_eq!(first, second);
    assert_eq!(first % (2 * 1024 * 1024), 0);
    assert!(first + 0x20_0000 <= 512 * 1024 * 1024 * 1024);
}

#[test]
fn rejects_invalid_kaslr_geometry() {
    assert_eq!(
        kaslr::select_offset(1, 0, 0x4000_0000, 0x20_0000),
        Err(Error::InvalidImage)
    );
    assert_eq!(
        kaslr::select_offset(1, 0x1000, 0x4000_0000, 0x30_0000),
        Err(Error::InvalidAlignment)
    );
    assert_eq!(
        kaslr::select_offset(1, 0x8000_0000, 0x4000_0000, 0x20_0000),
        Err(Error::ImageTooLarge)
    );
}
