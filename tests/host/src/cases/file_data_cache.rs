// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use alloc::sync::{Arc, Weak};
use core::num::NonZeroU64;
use std::sync::{Barrier, Mutex};

use crate::file_data_cache::{
    CacheAccess, CacheError, CacheKey, CachedPage, ContentRevision, FileDataCache, FileIdentity,
    FilePageIndex, FilesystemGeneration, LoadReservation, Maintenance, NodeIdentity, ReclaimCursor,
    Refill,
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
    CacheKey::new(
        FileIdentity::new(filesystem, node, ContentRevision::new(NonZeroU64::MIN)),
        FilePageIndex::new(page),
    )
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
    let published =
        crate::require_ok(load.fill_and_publish(|| Ok([0x5a_u8; 16]), |_| Refill::Recreate));
    let hit = require_hit(crate::require_ok(cache.access(selected)));
    assert_eq!(published.value(), hit.value());
    assert_eq!(hit.value()[4], 0x5a);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.clean, 1);
    assert_eq!(snapshot.loading, 0);
    assert_eq!(snapshot.hits, 1);
    assert_eq!(snapshot.misses, 1);
    assert_eq!(snapshot.in_progress, 1);
    assert_eq!(snapshot.live_payloads, 1);
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
            let _ = crate::require_ok(load.fill_and_publish(|| Ok(34), |_| Refill::Recreate));
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
    assert_eq!(snapshot.live_payloads, 0);

    let retry = require_load(crate::require_ok(cache.access(selected)));
    let page = crate::require_ok(retry.fill_and_publish(|| Ok(81), |_| Refill::Recreate));
    assert_eq!(*page.value(), 81);
}

#[test]
fn stale_fill_cannot_replace_a_recycled_slot() {
    // The invalidated old loader still owns a payload permit while its slot
    // is reused. Both allocations must fit, including the stale completion.
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    let selected = key(2, 4, 6);
    let stale = require_load(crate::require_ok(cache.access(selected)));
    stale.invalidate_slot_for_test();

    let replacement = require_load(crate::require_ok(cache.access(selected)));
    let replacement =
        crate::require_ok(replacement.fill_and_publish(|| Ok(22), |_| Refill::Recreate));
    let failure = match stale.fill_and_publish(|| Ok(11), |_| Refill::Recreate) {
        Ok(_) => panic!("stale fill unexpectedly published"),
        Err(failure) => failure,
    };
    assert_eq!(failure, CacheError::StaleLoad);
    assert_eq!(cache.snapshot().live_payloads, 1);
    assert_eq!(*replacement.value(), 22);
    let hit = require_hit(crate::require_ok(cache.access(selected)));
    assert_eq!(*hit.value(), 22);
}

#[test]
fn clean_eviction_preserves_an_active_reader_and_its_budget() {
    let cache = crate::require_ok(FileDataCache::<Arc<u64>>::try_new(1));
    let first_key = key(1, 1, 0);
    let second_key = key(1, 2, 0);
    let first_payload = Arc::new(11);
    let first = crate::require_ok(
        require_load(crate::require_ok(cache.access(first_key)))
            .fill_and_publish(|| Ok(first_payload.clone()), |_| Refill::Recreate),
    );
    assert!(matches!(
        crate::require_ok(cache.access(second_key)),
        CacheAccess::CapacityBusy
    ));
    assert_eq!(**first.value(), 11);
    assert_eq!(Arc::strong_count(&first_payload), 2);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.capacity, 1);
    assert_eq!(snapshot.clean, 0);
    assert_eq!(snapshot.loading, 0);
    assert_eq!(snapshot.live_payloads, 1);
    assert_eq!(snapshot.evictions, 1);
    drop(first);
    assert_eq!(Arc::strong_count(&first_payload), 1);
    assert_eq!(cache.snapshot().live_payloads, 0);
    let second = crate::require_ok(
        require_load(crate::require_ok(cache.access(second_key)))
            .fill_and_publish(|| Ok(Arc::new(22)), |_| Refill::Recreate),
    );
    assert_eq!(**second.value(), 22);
    assert_eq!(cache.snapshot().live_payloads, 1);
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
    assert_eq!(snapshot.live_payloads, 0);
}

#[test]
fn lookup_miss_does_not_reserve_or_evict() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
    let selected = key(1, 1, 0);
    drop(crate::require_ok(
        require_load(crate::require_ok(cache.access(selected)))
            .fill_and_publish(|| Ok(7), |_| Refill::Recreate),
    ));
    assert!(crate::require_ok(cache.lookup(key(1, 2, 0))).is_none());
    let snapshot = cache.snapshot();
    assert_eq!(
        (snapshot.clean, snapshot.loading, snapshot.live_payloads),
        (1, 0, 1)
    );
    assert_eq!(snapshot.evictions, 0);
    assert_eq!(
        *crate::require_some(crate::require_ok(cache.lookup(selected))).value(),
        7
    );
}

#[test]
fn rejected_publication_destroys_payload_before_releasing_its_permit() {
    let cache = Arc::new(crate::require_ok(FileDataCache::<ObservedPage>::try_new(1)));
    let drops = Arc::new(Mutex::new(Vec::new()));
    let load = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    cache.fail_next_publication_for_test();
    let failure =
        match load.fill_and_publish(|| Ok(observed(&cache, &drops, 9)), |_| Refill::Recreate) {
            Ok(_) => panic!("injected publication rejection was ignored"),
            Err(failure) => failure,
        };
    assert_eq!(failure, CacheError::Allocation);
    assert_eq!(*crate::require_ok(drops.lock()), vec![(9, 1)]);
    assert_eq!(cache.snapshot().loading, 0);
    assert_eq!(cache.snapshot().live_payloads, 0);
    let retry = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    assert_eq!(cache.snapshot().live_payloads, 1);
    drop(retry);
    assert_eq!(cache.snapshot().live_payloads, 0);
}

#[test]
fn payload_can_outlive_its_cache_owner() {
    let page = {
        let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
        crate::require_ok(
            require_load(crate::require_ok(cache.access(key(1, 1, 0))))
                .fill_and_publish(|| Ok(55), |_| Refill::Recreate),
        )
    };
    assert_eq!(*page.value(), 55);
    drop(page);
}

type DropLog = Arc<Mutex<Vec<(u8, usize)>>>;

struct ObservedPage {
    value: u8,
    cache: Weak<FileDataCache<ObservedPage>>,
    drops: DropLog,
}

impl Drop for ObservedPage {
    fn drop(&mut self) {
        let live = self
            .cache
            .upgrade()
            .map_or(0, |cache| cache.snapshot().live_payloads);
        crate::require_ok(self.drops.lock()).push((self.value, live));
    }
}

fn observed(cache: &Arc<FileDataCache<ObservedPage>>, drops: &DropLog, value: u8) -> ObservedPage {
    ObservedPage {
        value,
        cache: Arc::downgrade(cache),
        drops: drops.clone(),
    }
}

#[test]
fn exclusive_reuse_preserves_vector_and_shared_allocation() {
    let cache = crate::require_ok(FileDataCache::<Vec<u8>>::try_new(1));
    let first = crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 1, 0)))).fill_and_publish(
            || Ok(vec![11; 4096]),
            |_| panic!("fresh page unexpectedly reused"),
        ),
    );
    let header_value = first.value() as *const Vec<u8>;
    let bytes = first.value().as_ptr();
    drop(first);
    let replacement = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    assert_eq!(cache.snapshot().live_payloads, 1);
    let second = crate::require_ok(replacement.fill_and_publish(
        || panic!("exclusive page unnecessarily reconstructed"),
        |page| {
            assert!(page.capacity() >= 37);
            page.clear();
            page.extend_from_slice(&[22; 37]);
            Refill::Ready
        },
    ));
    assert_eq!(second.value() as *const Vec<u8>, header_value);
    assert_eq!(second.value().as_ptr(), bytes);
    assert_eq!(second.value(), &[22; 37]);
    assert_eq!(cache.snapshot().payload_creations, 1);
    assert_eq!(cache.snapshot().payload_reuses, 1);
    assert_eq!(cache.snapshot().live_payloads, 1);
}

#[test]
fn fresh_factory_failure_rolls_back_admission_without_a_payload() {
    let cache = crate::require_ok(FileDataCache::<Vec<u8>>::try_new(1));
    let load = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    let result = load.fill_and_publish(
        || {
            assert_eq!(cache.snapshot().live_payloads, 1);
            Err(CacheError::Allocation)
        },
        |_| panic!("fresh allocation attempted reuse"),
    );
    assert!(matches!(result, Err(CacheError::Allocation)));
    assert_eq!(cache.snapshot().live_payloads, 0);
    assert_eq!(cache.snapshot().loading, 0);
    assert_eq!(cache.snapshot().payload_creations, 0);
}

#[test]
fn recreate_drops_old_payload_before_factory_and_keeps_the_same_permit() {
    let cache = Arc::new(crate::require_ok(FileDataCache::<ObservedPage>::try_new(1)));
    let drops = Arc::new(Mutex::new(Vec::new()));
    drop(crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 1, 0))))
            .fill_and_publish(|| Ok(observed(&cache, &drops, 1)), |_| Refill::Recreate),
    ));
    let load = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    let result = load.fill_and_publish(
        || {
            assert_eq!(*crate::require_ok(drops.lock()), vec![(1, 1)]);
            assert_eq!(cache.snapshot().live_payloads, 1);
            Err(CacheError::Allocation)
        },
        |_| Refill::Recreate,
    );
    assert!(matches!(result, Err(CacheError::Allocation)));
    assert_eq!(cache.snapshot().live_payloads, 0);
    assert_eq!(cache.snapshot().loading, 0);
    assert_eq!(*crate::require_ok(drops.lock()), vec![(1, 1)]);
}

#[test]
fn dropping_unfilled_recycled_reservation_destroys_bytes_outside_cache_lock() {
    let cache = Arc::new(crate::require_ok(FileDataCache::<ObservedPage>::try_new(1)));
    let drops = Arc::new(Mutex::new(Vec::new()));
    drop(crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 1, 0))))
            .fill_and_publish(|| Ok(observed(&cache, &drops, 3)), |_| Refill::Recreate),
    ));
    let load = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    assert!(crate::require_ok(drops.lock()).is_empty());
    drop(load);
    assert_eq!(*crate::require_ok(drops.lock()), vec![(3, 1)]);
    assert_eq!(cache.snapshot().live_payloads, 0);
    assert_eq!(cache.snapshot().loading, 0);
    assert!(crate::require_ok(cache.lookup(key(1, 1, 0))).is_none());
    assert!(crate::require_ok(cache.lookup(key(1, 2, 0))).is_none());
}

#[test]
fn failed_recycled_publication_drops_filled_payload_before_its_permit() {
    let cache = Arc::new(crate::require_ok(FileDataCache::<ObservedPage>::try_new(1)));
    let drops = Arc::new(Mutex::new(Vec::new()));
    drop(crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 1, 0))))
            .fill_and_publish(|| Ok(observed(&cache, &drops, 1)), |_| Refill::Recreate),
    ));
    let load = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    cache.fail_next_publication_for_test();
    let result = load.fill_and_publish(
        || panic!("recycled payload recreated"),
        |page| {
            page.value = 2;
            Refill::Ready
        },
    );
    assert!(matches!(result, Err(CacheError::Allocation)));
    assert_eq!(*crate::require_ok(drops.lock()), vec![(2, 1)]);
    assert_eq!(cache.snapshot().live_payloads, 0);
    assert_eq!(cache.snapshot().loading, 0);
}

#[test]
fn stale_recycled_completion_or_drop_cannot_replace_a_new_payload() {
    for complete in [false, true] {
        let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
        drop(crate::require_ok(
            require_load(crate::require_ok(cache.access(key(1, 1, 0))))
                .fill_and_publish(|| Ok(11), |_| Refill::Recreate),
        ));
        let held = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
        let stale = require_load(crate::require_ok(cache.access(key(1, 3, 0))));
        drop(held);
        stale.invalidate_slot_for_test();
        let replacement = crate::require_ok(
            require_load(crate::require_ok(cache.access(key(1, 3, 0))))
                .fill_and_publish(|| Ok(33), |_| Refill::Recreate),
        );
        if complete {
            let result = stale.fill_and_publish(
                || panic!("recycled payload recreated"),
                |value| {
                    *value = 22;
                    Refill::Ready
                },
            );
            assert!(matches!(result, Err(CacheError::StaleLoad)));
        } else {
            drop(stale);
        }
        assert_eq!(*replacement.value(), 33);
        assert_eq!(
            *require_hit(crate::require_ok(cache.access(key(1, 3, 0)))).value(),
            33
        );
        assert_eq!(cache.snapshot().live_payloads, 1);
    }
}

fn publish_number(cache: &FileDataCache<u64>, node: u64) -> CachedPage<u64> {
    crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, node, 0))))
            .fill_and_publish(|| Ok(node), |_| Refill::Recreate),
    )
}

#[test]
fn table_growth_preserves_bytes_and_original_eviction_order() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    drop(publish_number(&cache, 1));
    drop(publish_number(&cache, 2));
    drop(require_hit(crate::require_ok(cache.access(key(1, 1, 0)))));
    assert_eq!(cache.maintain_capacity(4), Ok(Maintenance::Resized));
    assert_eq!(cache.usage().capacity, 4);
    assert_eq!(cache.usage().clean, 2);
    drop(publish_number(&cache, 3));
    drop(publish_number(&cache, 4));
    drop(publish_number(&cache, 5));
    assert!(crate::require_ok(cache.lookup(key(1, 2, 0))).is_none());
    for node in [1, 3, 4, 5] {
        assert_eq!(
            *require_hit(crate::require_ok(cache.access(key(1, node, 0)))).value(),
            node
        );
    }
    assert!(cache.usage().growth_requested);
    cache.acknowledge_growth();
    assert!(!cache.usage().growth_requested);
}

#[test]
fn readers_bypass_a_parked_table_while_a_victim_destructor_is_running() {
    struct DropGate {
        entered: Barrier,
        release: Barrier,
    }
    struct Page {
        value: u8,
        gate: Option<Arc<DropGate>>,
    }
    impl Drop for Page {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.entered.wait();
                gate.release.wait();
            }
        }
    }

    let cache = crate::require_ok(FileDataCache::try_new(2));
    let gate = Arc::new(DropGate {
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    drop(crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 1, 0)))).fill_and_publish(
            || {
                Ok(Page {
                    value: 11,
                    gate: Some(gate.clone()),
                })
            },
            |_| Refill::Recreate,
        ),
    ));
    let pinned = crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 2, 0)))).fill_and_publish(
            || {
                Ok(Page {
                    value: 22,
                    gate: None,
                })
            },
            |_| Refill::Recreate,
        ),
    );

    let (completed, result) = std::sync::mpsc::channel();
    let observed = std::thread::scope(|scope| {
        let maintenance = scope.spawn(|| cache.maintain_capacity(1));
        // Shrink has parked the table and is destroying its oldest victim.
        // Keeping that destructor paused creates a deterministic overlap.
        gate.entered.wait();
        scope.spawn(|| {
            let absent = crate::require_ok(cache.lookup(key(1, 1, 0))).is_none()
                && crate::require_ok(cache.lookup(key(1, 2, 0))).is_none();
            let bypass = matches!(
                crate::require_ok(cache.access(key(1, 3, 0))),
                CacheAccess::CapacityBusy
            );
            crate::require_ok(completed.send((absent, bypass)));
        });
        let observed = result.recv_timeout(std::time::Duration::from_secs(5));
        // Always release the destructor before reporting a failed assertion.
        // A regression that holds the cache lock can then finish and fail this
        // test instead of leaving the host suite blocked forever.
        gate.release.wait();
        assert_eq!(
            crate::require_ok(maintenance.join()),
            Ok(Maintenance::Resized)
        );
        observed
    });
    assert_eq!(crate::require_ok(observed), (true, true));
    assert_eq!(pinned.value().value, 22);
    assert_eq!(cache.usage().capacity, 1);
    assert!(crate::require_ok(cache.lookup(key(1, 1, 0))).is_none());
    assert_eq!(
        require_hit(crate::require_ok(cache.access(key(1, 2, 0))))
            .value()
            .value,
        22
    );
    drop(pinned);
    let replacement = crate::require_ok(
        require_load(crate::require_ok(cache.access(key(1, 3, 0)))).fill_and_publish(
            || {
                Ok(Page {
                    value: 33,
                    gate: None,
                })
            },
            |_| Refill::Recreate,
        ),
    );
    assert_eq!(replacement.value().value, 33);
}

#[test]
fn pending_maintenance_stops_new_loaders_until_existing_fill_finishes() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(1));
    let load = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    assert_eq!(cache.maintain_capacity(2), Ok(Maintenance::Busy));
    assert!(cache.usage().maintenance_pending);
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    drop(crate::require_ok(
        load.fill_and_publish(|| Ok(1), |_| Refill::Recreate),
    ));
    assert_eq!(cache.usage().loading, 0);
    assert_eq!(cache.maintain_capacity(2), Ok(Maintenance::Resized));
    assert!(!cache.usage().maintenance_pending);
    drop(publish_number(&cache, 2));
    assert_eq!(cache.usage().clean, 2);
}

#[test]
fn cancelling_maintenance_reopens_only_its_own_gate() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    let load = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    assert_eq!(cache.maintain_capacity(4), Ok(Maintenance::Busy));
    cache.set_admission(false);
    cache.cancel_maintenance();
    assert!(!cache.usage().maintenance_pending);
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    cache.set_admission(true);
    let next = require_load(crate::require_ok(cache.access(key(1, 2, 0))));
    drop(next);
    drop(load);
    assert_eq!(cache.usage().loading, 0);
}

#[test]
fn failed_resize_restores_original_table_without_reopening_pressure_gate() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    drop(publish_number(&cache, 1));
    cache.set_admission(false);
    cache.fail_next_maintenance_for_test();
    assert_eq!(cache.maintain_capacity(4), Err(CacheError::Allocation));
    assert_eq!(cache.usage().capacity, 2);
    assert_eq!(cache.usage().clean, 1);
    assert!(!cache.usage().maintenance_pending);
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(key(1, 1, 0)))).value(),
        1
    );
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    cache.set_admission(true);
    drop(publish_number(&cache, 2));
}

#[test]
fn empty_shrink_failure_disables_cache_and_keeps_detached_reader_charged() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let pin = publish_number(&cache, 1);
    assert_eq!(cache.reclaim_batch(64), 1);
    cache.set_admission(false);
    cache.fail_next_maintenance_for_test();
    assert_eq!(cache.maintain_capacity(1), Err(CacheError::Allocation));
    let usage = cache.usage();
    assert_eq!(usage.capacity, 0);
    assert_eq!(usage.metadata_pages, 0);
    assert_eq!(usage.live_payloads, 1);
    assert!(!usage.maintenance_pending);
    assert_eq!(*pin.value(), 1);
    cache.set_admission(true);
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    assert!(cache.usage().growth_requested);
    assert_eq!(cache.maintain_capacity(2), Ok(Maintenance::Resized));
    drop(publish_number(&cache, 2));
    assert_eq!(cache.usage().live_payloads, 2);
    drop(pin);
    assert_eq!(cache.usage().live_payloads, 1);
}

#[test]
fn shrinking_reclaims_cold_entries_but_not_their_reader_pins() {
    let cache = crate::require_ok(FileDataCache::try_new(3));
    let cold_pin = publish_number(&cache, 1);
    drop(publish_number(&cache, 2));
    drop(publish_number(&cache, 3));
    assert_eq!(cache.maintain_capacity(1), Ok(Maintenance::Resized));
    assert_eq!(cache.usage().capacity, 1);
    assert_eq!(cache.usage().clean, 1);
    assert_eq!(cache.usage().live_payloads, 2);
    assert_eq!(*cold_pin.value(), 1);
    assert!(crate::require_ok(cache.lookup(key(1, 1, 0))).is_none());
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(key(1, 3, 0)))).value(),
        3
    );
    drop(cold_pin);
    assert_eq!(cache.usage().live_payloads, 1);
}

#[test]
fn emergency_reclaim_skips_pins_while_worker_can_detach_them() {
    let cache = crate::require_ok(FileDataCache::try_new(3));
    let pin = publish_number(&cache, 1);
    drop(publish_number(&cache, 2));
    drop(publish_number(&cache, 3));
    assert_eq!(cache.reclaimable_pages(), Some(2));
    assert_eq!(cache.try_reclaim_batch(64), 2);
    assert_eq!(cache.usage().clean, 1);
    assert_eq!(cache.usage().live_payloads, 1);
    assert_eq!(cache.reclaimable_pages(), Some(0));
    assert_eq!(cache.reclaim_batch(64), 1);
    assert_eq!(cache.usage().clean, 0);
    assert_eq!(cache.usage().live_payloads, 1);
    assert_eq!(*pin.value(), 1);
    drop(pin);
    assert_eq!(cache.usage().live_payloads, 0);
}

#[test]
fn pressure_rejects_recycling_as_well_as_fresh_admission() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    drop(publish_number(&cache, 1));
    cache.set_admission(false);
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    assert_eq!(cache.usage().clean, 1);
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(key(1, 1, 0)))).value(),
        1
    );
}

#[test]
fn nested_reclaim_pauses_exclude_admission_and_growth_until_final_retry_finishes() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    let existing = require_load(crate::require_ok(cache.access(key(1, 1, 0))));
    let first = cache.pause_admission();
    let second = cache.pause_admission();
    drop(crate::require_ok(
        existing.fill_and_publish(|| Ok(1), |_| Refill::Recreate),
    ));
    cache.set_admission(true);
    cache.cancel_maintenance();
    assert_eq!(cache.maintain_capacity(4), Ok(Maintenance::Busy));
    assert!(!cache.usage().maintenance_pending);
    drop(first);
    assert!(cache.usage().admission_paused);
    assert!(matches!(
        crate::require_ok(cache.access(key(1, 2, 0))),
        CacheAccess::CapacityBusy
    ));
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(key(1, 1, 0)))).value(),
        1
    );
    drop(second);
    assert!(!cache.usage().admission_paused);
    drop(publish_number(&cache, 2));
    assert_eq!(cache.maintain_capacity(4), Ok(Maintenance::Resized));
}

#[test]
fn scheduled_reclaim_scans_pins_and_loaders_once_and_matches_outside_the_lock() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(4));
    let pinned = publish_number(&cache, 1);
    drop(publish_number(&cache, 2));
    drop(publish_number(&cache, 3));
    let loading = require_load(crate::require_ok(cache.access(key(1, 4, 0))));
    let pause = cache.pause_admission();
    let mut cursor = ReclaimCursor::new();
    let mut visited = Vec::new();
    let first = cache.reclaim_scan(&mut cursor, 2, |value| {
        assert!(cache.usage().admission_paused);
        visited.push(*value);
        *value == 2
    });
    assert_eq!(
        (first.inspected, first.detached, first.finished),
        (2, 1, false)
    );
    let second = cache.reclaim_scan(&mut cursor, 64, |value| {
        visited.push(*value);
        false
    });
    assert_eq!(
        (second.inspected, second.detached, second.finished),
        (2, 0, true)
    );
    assert_eq!(visited, [1, 2, 3]);
    assert_eq!(cache.reclaim_scan(&mut cursor, 64, |_| true).inspected, 0);
    let mut all = ReclaimCursor::new();
    let remaining = cache.reclaim_scan(&mut all, 64, |_| true);
    assert_eq!(
        (remaining.inspected, remaining.detached, remaining.finished),
        (4, 2, true)
    );
    assert_eq!(*pinned.value(), 1);
    assert_eq!(cache.usage().loading, 1);
    assert_eq!(cache.usage().live_payloads, 2);
    drop(loading);
    drop(pinned);
    drop(pause);
    assert_eq!(cache.usage().live_payloads, 0);
}

#[test]
fn reclaim_matcher_cannot_detach_a_replacement_published_in_the_same_slot() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    drop(publish_number(&cache, 1));
    let mut cursor = ReclaimCursor::new();
    let scan = cache.reclaim_scan(&mut cursor, 64, |value| {
        assert_eq!(*value, 1);
        assert_eq!(cache.reclaim_batch(1), 1);
        drop(publish_number(&cache, 2));
        true
    });
    assert_eq!((scan.inspected, scan.detached, scan.finished), (2, 0, true));
    assert_eq!(
        *require_hit(crate::require_ok(cache.access(key(1, 2, 0)))).value(),
        2
    );
    assert_eq!(cache.usage().live_payloads, 1);
}

#[test]
fn a_reclaim_cursor_finishes_when_maintenance_changes_its_table() {
    let cache = crate::require_ok(FileDataCache::<u64>::try_new(2));
    drop(publish_number(&cache, 1));
    let mut cursor = ReclaimCursor::new();
    assert_eq!(cache.reclaim_scan(&mut cursor, 1, |_| false).inspected, 1);
    assert_eq!(cache.maintain_capacity(4), Ok(Maintenance::Resized));
    let scan = cache.reclaim_scan(&mut cursor, 64, |_| panic!("obsolete table scan"));
    assert!(scan.finished);
    assert_eq!(scan.detached, 0);
    assert_eq!(cache.usage().clean, 1);
}
