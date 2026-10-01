// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Collision-limited migration keeps independent reader ownership intact.

use alloc::vec::Vec;
use core::num::NonZeroU64;

use super::super::{
    CacheAccess, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePageIndex,
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
