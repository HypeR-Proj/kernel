// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Collision-limited migration keeps independent reader ownership intact.

use alloc::vec::Vec;
use core::num::NonZeroU64;

use super::super::{
    CacheAccess, CacheError, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePageIndex,
    FilesystemGeneration, NodeIdentity, Refill, index,
};
use super::Maintenance;

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

#[test]
fn shrinking_merged_buckets_discards_excess_without_losing_a_reader_pin() {
    let count = index::MAX_CHAIN + 6;
    let keys: Vec<_> = (0..65536)
        .map(key)
        .filter(|key| key.bucket(255) == 0)
        .take(count)
        .collect();
    assert_eq!(keys.len(), count);
    // The old 512 buckets split these keys between two chains; the new 256
    // buckets merge them into one chain that exceeds the fixed lookup bound.
    for bucket in [0, 256] {
        assert!(keys.iter().filter(|key| key.bucket(511) == bucket).count() <= index::MAX_CHAIN);
    }
    let cache = FileDataCache::try_new(256).unwrap_or_else(|error| panic!("cache: {error:?}"));
    let mut pin = None;
    for (index, &key) in keys.iter().enumerate() {
        let load = match cache.access(key) {
            Ok(CacheAccess::Load(load)) => load,
            _ => panic!("unsaturated old bucket rejected a load"),
        };
        let page = load
            .fill_and_publish(|| Ok(index), |_| Refill::Recreate)
            .unwrap_or_else(|error| panic!("publication: {error:?}"));
        if index + 1 == count {
            pin = Some(page);
        }
    }
    assert_eq!(cache.maintain_capacity(128), Ok(Maintenance::Resized));
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.capacity, 128);
    assert_eq!(snapshot.clean, index::MAX_CHAIN);
    assert_eq!(snapshot.live_payloads, index::MAX_CHAIN + 1);
    assert_eq!(snapshot.evictions, 6);
    for (index, &key) in keys.iter().enumerate() {
        let page = cache
            .lookup(key)
            .unwrap_or_else(|error| panic!("lookup: {error:?}"));
        if index < index::MAX_CHAIN {
            assert_eq!(page.as_ref().map(|page| *page.value()), Some(index));
        } else {
            assert!(page.is_none());
        }
    }
    assert_eq!(pin.as_ref().map(|page| *page.value()), Some(count - 1));
    drop(pin);
    assert_eq!(cache.snapshot().live_payloads, index::MAX_CHAIN);
    assert_eq!(cache.maintain_capacity(256), Ok(Maintenance::Resized));
    assert_eq!(cache.snapshot().clean, index::MAX_CHAIN);
    assert!(
        cache
            .lookup(keys[count - 1])
            .unwrap_or_else(|error| panic!("lookup: {error:?}"))
            .is_none()
    );
}

#[test]
fn shrinking_preserves_mapped_keys_even_when_buckets_merge() {
    let keys: Vec<_> = (0..65536)
        .map(key)
        .filter(|key| key.bucket(255) == 0)
        .take(index::MAX_CHAIN + 6)
        .collect();
    let cache = crate::require_ok(FileDataCache::try_new(256));
    let mut mappings = Vec::new();
    for (value, &key) in keys.iter().enumerate() {
        let load = match crate::require_ok(cache.access(key)) {
            CacheAccess::Load(load) => load,
            _ => panic!("old buckets unexpectedly saturated"),
        };
        drop(crate::require_ok(
            load.fill_and_publish(|| Ok(value), |_| Refill::Recreate),
        ));
        mappings.push(crate::require_some(crate::require_ok(
            cache.lookup_mapping(key),
        )));
    }
    assert_eq!(cache.maintain_capacity(128), Ok(Maintenance::Busy));
    for (value, &key) in keys.iter().enumerate() {
        let mapped = crate::require_some(crate::require_ok(cache.lookup_mapping(key)));
        assert!(core::ptr::eq(mapped.value(), mappings[value].value()));
    }
    // One old bucket can now shrink while the still mapped canonical page survives.
    let retained = mappings.remove(mappings.len() - 1);
    drop(mappings);
    assert_eq!(cache.maintain_capacity(128), Ok(Maintenance::Resized));
    let mapped = crate::require_some(crate::require_ok(
        cache.lookup_mapping(keys[keys.len() - 1]),
    ));
    assert!(core::ptr::eq(mapped.value(), retained.value()));
    assert_eq!(cache.usage().capacity, 128);
}

#[test]
fn fully_mapped_table_does_not_allocate_an_identical_replacement() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let mut mappings = Vec::new();
    for i in 0..2 {
        let load = match crate::require_ok(cache.access(key(i))) {
            CacheAccess::Load(load) => load,
            _ => panic!("empty cache did not admit a page"),
        };
        drop(crate::require_ok(
            load.fill_and_publish(|| Ok(i), |_| Refill::Recreate),
        ));
        mappings.push(crate::require_some(crate::require_ok(
            cache.lookup_mapping(key(i)),
        )));
    }
    cache.fail_next_maintenance_for_test();
    assert_eq!(cache.maintain_capacity(1), Ok(Maintenance::Unchanged));
    assert_eq!(cache.usage().capacity, 2);
    assert_eq!(cache.maintain_capacity(4), Err(CacheError::Allocation));
    for (i, mapped) in mappings.iter().enumerate() {
        let current = crate::require_some(crate::require_ok(cache.lookup_mapping(key(i as u64))));
        assert!(core::ptr::eq(current.value(), mapped.value()));
    }
}
