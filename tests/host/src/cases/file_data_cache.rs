// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use alloc::sync::Arc;
use core::num::NonZeroU64;
use std::sync::Barrier;

use crate::file_data_cache::{
    CacheAccess, CacheError, CacheKey, CachedPage, FileDataCache, FilePageIndex,
    FilesystemGeneration, LoadReservation, NodeIdentity,
};

fn key(filesystem: u64, node: u64, page: u64) -> CacheKey {
    let filesystem = match NonZeroU64::new(filesystem) {
        Some(value) => FilesystemGeneration::new(value),
        None => panic!("test filesystem generation must be nonzero"),
    };
    let node = match NonZeroU64::new(node) {
        Some(value) => NodeIdentity::new(value),
        None => panic!("test node identity must be nonzero"),
    };
    CacheKey::new(filesystem, node, FilePageIndex::new(page))
}

fn require_load<Page>(access: CacheAccess<'_, Page>) -> LoadReservation<'_, Page> {
    match access {
        CacheAccess::Load(load) => load,
        CacheAccess::Hit(_) | CacheAccess::LoadInProgress | CacheAccess::CapacityBusy => {
            panic!("cache access did not reserve a load")
        }
    }
}

fn require_hit<Page>(access: CacheAccess<'_, Page>) -> CachedPage<Page> {
    match access {
        CacheAccess::Hit(page) => page,
        CacheAccess::LoadInProgress | CacheAccess::Load(_) | CacheAccess::CapacityBusy => {
            panic!("cache access did not hit")
        }
    }
}

#[test]
fn one_loader_publishes_one_immutable_shared_page() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let selected = key(1, 7, 3);
    let load = require_load(crate::require_ok(cache.access(selected)));
    assert!(matches!(
        crate::require_ok(cache.access(selected)),
        CacheAccess::LoadInProgress
    ));
    let published = crate::require_ok(load.publish([0x5a_u8; 16]));
    let hit = require_hit(crate::require_ok(cache.access(selected)));
    assert_eq!(published.value(), hit.value());
    assert_eq!(hit.value()[4], 0x5a);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.clean, 1);
    assert_eq!(snapshot.loading, 0);
    assert_eq!(snapshot.hits, 1);
    assert_eq!(snapshot.misses, 1);
    assert_eq!(snapshot.in_progress, 1);
}

#[test]
fn concurrent_lookup_observes_the_single_loader() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
    let selected = key(8, 13, 21);
    let reserved = Barrier::new(2);
    let observed = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let load = require_load(crate::require_ok(cache.access(selected)));
            reserved.wait();
            observed.wait();
            let _ = crate::require_ok(load.publish(34));
        });
        scope.spawn(|| {
            reserved.wait();
            assert!(matches!(
                crate::require_ok(cache.access(selected)),
                CacheAccess::LoadInProgress
            ));
            observed.wait();
        });
    });
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(selected))).value(),
        34
    );
}

#[test]
fn failed_fill_rolls_back_and_allows_a_new_generation() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
    let selected = key(4, 9, 0);
    let load = require_load(crate::require_ok(cache.access(selected)));
    crate::require_ok(load.abort());
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.clean, 0);
    assert_eq!(snapshot.loading, 0);

    let retry = require_load(crate::require_ok(cache.access(selected)));
    let page = crate::require_ok(retry.publish(81));
    assert_eq!(*page.value(), 81);
}

#[test]
fn stale_fill_cannot_replace_a_recycled_slot() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
    let selected = key(2, 4, 6);
    let stale = require_load(crate::require_ok(cache.access(selected)));
    stale.invalidate_slot_for_test();

    let replacement = require_load(crate::require_ok(cache.access(selected)));
    let replacement = crate::require_ok(replacement.publish(22));
    let failure = match stale.publish(11) {
        Ok(_) => panic!("stale fill unexpectedly published"),
        Err(failure) => failure,
    };
    assert_eq!(failure.cause(), CacheError::StaleLoad);
    assert_eq!(failure.into_page(), 11);
    assert_eq!(*replacement.value(), 22);
    let hit = require_hit(crate::require_ok(cache.access(selected)));
    assert_eq!(*hit.value(), 22);
}

#[test]
fn clean_eviction_preserves_an_active_reader() {
    let cache = crate::require_ok(FileDataCache::<Arc<u64>>::try_new(1));
    let first_key = key(1, 1, 0);
    let second_key = key(1, 2, 0);
    let first_payload = Arc::new(11);
    let first = crate::require_ok(
        require_load(crate::require_ok(cache.access(first_key))).publish(first_payload.clone()),
    );
    let second = crate::require_ok(
        require_load(crate::require_ok(cache.access(second_key))).publish(Arc::new(22)),
    );
    assert_eq!(**first.value(), 11);
    assert_eq!(**second.value(), 22);
    assert_eq!(Arc::strong_count(&first_payload), 2);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.capacity, 1);
    assert_eq!(snapshot.clean, 1);
    assert_eq!(snapshot.evictions, 1);
}

#[test]
fn all_loading_slots_report_capacity_busy_without_eviction() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    let first = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    let second = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 3, 0))),
        CacheAccess::CapacityBusy
    ));
    drop(first);
    drop(second);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.clean, 0);
    assert_eq!(snapshot.loading, 0);
    assert_eq!(snapshot.capacity_busy, 1);
}
