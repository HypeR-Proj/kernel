// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic tests for the allocation-free magazine ownership container.

#[path = "../../../../src/mm/allocator/heap/local_cache.rs"]
mod model;

use model::{MAGAZINE_STORAGE, Magazine, PushError};

#[test]
fn magazine_is_bounded_and_lifo() {
    let mut magazine = Magazine::new();
    assert!(magazine.is_empty());
    for value in 0..4 {
        assert_eq!(magazine.push(value, 4), Ok(()));
    }
    assert!(matches!(magazine.push(4, 4), Err(PushError::Full(4))));
    assert_eq!(magazine.len(), 4);
    assert_eq!(magazine.pop(), Ok(Some(3)));
    assert_eq!(magazine.pop(), Ok(Some(2)));
    assert_eq!(magazine.pop(), Ok(Some(1)));
    assert_eq!(magazine.pop(), Ok(Some(0)));
    assert_eq!(magazine.pop(), Ok(None));
}

#[test]
fn take_moves_each_value_exactly_once() {
    let mut source = Magazine::new();
    for value in 0..MAGAZINE_STORAGE {
        assert_eq!(source.push(value, MAGAZINE_STORAGE), Ok(()));
    }

    let mut batch = crate::require_ok(source.take(5));
    assert_eq!(source.len(), MAGAZINE_STORAGE - 5);
    assert_eq!(batch.len(), 5);
    for expected in MAGAZINE_STORAGE - 5..MAGAZINE_STORAGE {
        assert_eq!(batch.pop(), Ok(Some(expected)));
    }
    assert!(batch.is_empty());

    let remainder = crate::require_ok(source.take(usize::MAX));
    assert_eq!(remainder.len(), MAGAZINE_STORAGE - 5);
    assert!(source.is_empty());
}
