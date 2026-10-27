// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Identity reuse, generation wrap, and the final object-destruction boundary.

extern crate alloc;

use core::num::NonZeroU32;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[path = "../src/cases/capability_harness.rs"]
#[allow(dead_code, unused_imports)]
mod kernel;
pub(crate) use kernel::{accounting, authority, object};

use kernel::object::{KernelObject, KernelRef, ObjectKind, Scheduler};
use kernel::{ObjectCreationError, Rights, identity as allocator};

fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("required success, received {error:?}"),
    }
}

fn require_some<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("required a value"),
    }
}

fn slot(koid: u64) -> u32 {
    koid as u32
}

#[test]
fn koid_slots_recycle_with_advancing_generations() {
    let allocator = allocator::KoidAllocator::new();
    let mut observed = BTreeSet::new();
    for _ in 0..10_000 {
        let reservation = crate::require_ok(allocator.reserve());
        let koid = reservation.koid().get();
        assert_eq!(slot(koid), 1);
        assert!(observed.insert(koid));
    }
}

#[test]
fn koid_rollback_and_exhaustion_preserve_available_slots() {
    let allocator = allocator::KoidAllocator::with_last_slot_for_test();
    let last = crate::require_ok(allocator.reserve());
    let previous = last.koid();
    assert_eq!(slot(previous.get()), u32::MAX);
    assert!(matches!(
        allocator.reserve(),
        Err(ObjectCreationError::KoidExhausted)
    ));

    // Abandoning unpublished construction returns its reservation as well.
    drop(last);
    let mut recycled = crate::require_ok(allocator.reserve());
    assert_eq!(slot(recycled.koid().get()), slot(previous.get()));
    assert_ne!(recycled.koid(), previous);

    recycled.exhaust_generation_for_test();
    drop(recycled);
    let wrapped = crate::require_ok(allocator.reserve());
    assert_eq!(wrapped.koid(), previous);
    assert!(matches!(
        allocator.reserve(),
        Err(ObjectCreationError::KoidExhausted)
    ));
}

#[test]
fn koid_generation_wrap_returns_the_slot_without_disturbing_live_identities() {
    let allocator = allocator::KoidAllocator::new();
    let mut exhausted = crate::require_ok(allocator.reserve());
    let first = exhausted.koid();
    let available = crate::require_ok(allocator.reserve());
    let reusable_slot = slot(available.koid().get());
    let live = crate::require_ok(allocator.reserve());
    let live_koid = live.koid();
    exhausted.exhaust_generation_for_test();
    drop(available);
    drop(exhausted);
    let reused = crate::require_ok(allocator.reserve());
    assert_eq!(reused.koid(), first);
    let other = crate::require_ok(allocator.reserve());
    assert_eq!(slot(other.koid().get()), reusable_slot);
    assert_eq!(live.koid(), live_koid);
    assert_ne!(reused.koid(), live_koid);
    assert_ne!(other.koid(), live_koid);
}

#[test]
fn concurrent_koid_reservations_never_share_live_slots() {
    let allocator = allocator::KoidAllocator::new();
    let live = Mutex::new(BTreeSet::new());
    let identities = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let mut identities = Vec::new();
                    for _ in 0..1_000 {
                        let reservation = crate::require_ok(allocator.reserve());
                        let koid = reservation.koid().get();
                        assert!(crate::require_ok(live.lock()).insert(slot(koid)));
                        std::thread::yield_now();
                        assert!(crate::require_ok(live.lock()).remove(&slot(koid)));
                        identities.push(koid);
                        drop(reservation);
                    }
                    identities
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| crate::require_ok(worker.join()))
            .collect::<BTreeSet<_>>()
    });
    assert_eq!(identities.len(), 4_000);
    assert!(crate::require_ok(live.lock()).is_empty());
}

struct Probe;

#[test]
fn concurrent_koid_wrap_never_reuses_a_live_slot() {
    let allocator = allocator::KoidAllocator::new();
    let live = Mutex::new(BTreeSet::new());
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..1_000 {
                        let mut reservation = crate::require_ok(allocator.reserve());
                        let koid = reservation.koid().get();
                        assert_eq!(koid >> 32, 1);
                        assert!(crate::require_ok(live.lock()).insert(slot(koid)));
                        reservation.exhaust_generation_for_test();
                        std::thread::yield_now();
                        assert!(crate::require_ok(live.lock()).remove(&slot(koid)));
                        drop(reservation);
                    }
                })
            })
            .collect();
        for worker in workers {
            crate::require_ok(worker.join());
        }
    });
    assert!(crate::require_ok(live.lock()).is_empty());
}

impl kernel::object::private::Sealed for Probe {}

impl KernelObject for Probe {
    const KIND: ObjectKind = ObjectKind::for_test(NonZeroU32::MIN);
    const SUPPORTED_RIGHTS: Rights = Rights::NONE;
}

struct AllocateOnDrop(Arc<Mutex<Option<KernelRef<Probe, Scheduler>>>>);

impl kernel::object::private::Sealed for AllocateOnDrop {}

impl KernelObject for AllocateOnDrop {
    const KIND: ObjectKind = ObjectKind::for_test(NonZeroU32::MIN);
    const SUPPORTED_RIGHTS: Rights = Rights::NONE;
}

impl Drop for AllocateOnDrop {
    fn drop(&mut self) {
        let object = crate::require_ok(KernelRef::try_new_scheduler(Probe));
        *crate::require_ok(self.0.lock()) = Some(object);
    }
}

#[test]
fn koid_reuse_waits_for_final_payload_destruction_and_cannot_revive_weak_refs() {
    // This harness owns a separate production object allocator and reap queue;
    // the other tests in this module exercise local allocators only.
    let created_in_drop = Arc::new(Mutex::new(None));
    let object = crate::require_ok(KernelRef::try_new_scheduler(AllocateOnDrop(
        created_in_drop.clone(),
    )));
    let koid = object.koid().get();
    let retained = object.clone();
    let weak = object.downgrade_for_test();
    drop(object);
    let while_retained = crate::require_ok(KernelRef::try_new_scheduler(Probe));
    assert_ne!(slot(while_retained.koid().get()), slot(koid));
    assert!(kernel::weak_is_alive(&weak));

    drop(retained);
    assert!(!kernel::weak_can_upgrade(&weak));
    let before_reaping = crate::require_ok(KernelRef::try_new_scheduler(Probe));
    assert_ne!(slot(before_reaping.koid().get()), slot(koid));
    assert!(kernel::object::reap_one_final_object());
    let during_drop = crate::require_some(crate::require_ok(created_in_drop.lock()).take());
    assert_ne!(slot(during_drop.koid().get()), slot(koid));

    let after_reaping = crate::require_ok(KernelRef::try_new_scheduler(Probe));
    assert_eq!(slot(after_reaping.koid().get()), slot(koid));
    assert_ne!(after_reaping.koid().get(), koid);
    assert!(!kernel::weak_can_upgrade(&weak));
    drop((while_retained, before_reaping, during_drop, after_reaping));
    assert_eq!(kernel::object::reap_final_objects(16), 4);
}
