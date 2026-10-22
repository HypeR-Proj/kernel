// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Seeded state-machine fuzzing of the production handle implementation.
//! This exercises transactions, not Process accounting or scheduler interleavings.

extern crate alloc;

use core::num::NonZeroU32;
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// A separate test binary owns its reaper queue. Fuzzing must not reap objects
// used by parallel tests that inspect the deferred-destruction boundary.
#[path = "../src/cases/capability_harness.rs"]
#[allow(dead_code, unused_imports)]
mod kernel;

fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("expected success: {error:?}"),
    }
}

use kernel::capability::{
    HandleError, HandleFlags, HandleScanCursor, HandleTable, HandleTransferOperation,
    HandleTransferRequest, HandleTransferRoute, HandleValue, PreparedHandle, Rights,
};
use kernel::object::{
    KernelObject, KernelService, ObjectKind, ObjectRetirement, PublishableRef, TransferClass,
};

const TEST_KIND: ObjectKind = match NonZeroU32::new(0x7fff_ff01) {
    Some(value) => ObjectKind::for_test(value),
    None => panic!("test kind is nonzero"),
};

struct TestObject {
    value: u64,
    transitions: Arc<AtomicUsize>,
}
impl kernel::object::private::Sealed for TestObject {}
impl kernel::object::private::UserExportable for TestObject {}
impl KernelObject for TestObject {
    const KIND: ObjectKind = TEST_KIND;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = FULL;
    fn signal_source(&self) -> Option<kernel::object::signals::SignalSource<'_>> {
        Some(kernel::object::signals::SignalSource::for_test())
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.transitions.fetch_add(1, Ordering::Relaxed);
    }
}

fn object(value: u64, transitions: &Arc<AtomicUsize>) -> PublishableRef<TestObject, KernelService> {
    crate::require_ok(PublishableRef::try_new(TestObject {
        value,
        transitions: transitions.clone(),
    }))
}

fn prepared(owner: PublishableRef<TestObject, KernelService>, rights: Rights) -> PreparedHandle {
    crate::require_ok(PreparedHandle::try_from_new_object(
        owner.publication(),
        rights,
        HandleFlags::NONE,
    ))
}

fn remove_all(table: &mut HandleTable) {
    let mut cursor = crate::require_ok(table.begin_teardown());
    while let Some(closed) = table.remove_next(&mut cursor) {
        closed.complete();
    }
    table.finish_teardown(cursor);
}

const FULL: Rights = Rights::INSPECT
    .union(Rights::WAIT)
    .union(Rights::TRANSFER)
    .union(Rights::DUPLICATE);

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        // SplitMix64: every seed, including zero, produces a useful stream.
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn index(&mut self, length: usize) -> usize {
        self.next() as usize % length
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    handle: HandleValue,
    object: usize,
    rights: Rights,
}

#[derive(Default)]
struct Model {
    live: Vec<Entry>,
    stale: Vec<HandleValue>,
}

fn check(table: &HandleTable, model: &Model) {
    assert!(table.free_list_is_consistent_for_test());
    for entry in &model.live {
        let resolved = crate::require_ok(table.resolve::<TestObject>(entry.handle, Rights::NONE));
        assert_eq!(resolved.object().value, entry.object as u64);
        assert_eq!(resolved.rights(), entry.rights);
    }
    for value in &model.stale {
        assert_eq!(table.get_info(*value), Err(HandleError::InvalidHandle));
    }
    let mut cursor = Some(HandleScanCursor::from_token(0));
    let mut observed = Vec::new();
    while let Some(position) = cursor {
        let page = crate::require_ok(table.scan_handles(position));
        observed.extend(page.entries().map(|entry| entry.value));
        let next = page.next();
        if let Some(next) = next {
            assert!(
                next.token() > position.token(),
                "diagnostic cursor did not advance"
            );
        }
        assert!(observed.len() <= model.live.len());
        cursor = next;
    }
    let expected: Vec<_> = model.live.iter().map(|entry| entry.handle).collect();
    assert_eq!(observed.len(), expected.len());
    assert_eq!(
        observed
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        expected
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
    );
}

fn run(seed: u64, steps: usize) {
    let mut random = Random(seed);
    let mut tables = [
        HandleTable::new(seed),
        HandleTable::new(seed.rotate_left(17)),
    ];
    let mut models = [Model::default(), Model::default()];
    let mut objects = Vec::new();
    let transitions = Arc::new(AtomicUsize::new(0));
    let mut trace = std::collections::VecDeque::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for step in 0..steps {
            let side = random.index(2);
            let mut op = random.index(10);
            if models[side].live.is_empty() {
                op = 0;
            } else if models[side].live.len() >= 192 {
                op = 2;
            }
            if trace.len() == 32 {
                trace.pop_front();
            }
            trace.push_back((step, side, op, random.0));
            match op {
                0 => {
                    // Cross allocation-page boundaries, including maximum-size batches.
                    let n = [1, 2, 31, 32, 63, 64][random.index(6)];
                    let reservation = crate::require_ok(tables[side].reserve_batch(n));
                    let values = reservation.values().to_vec();
                    let mut handles = Vec::new();
                    for handle in values {
                        let id = objects.len();
                        let owner = object(id as u64, &transitions);
                        handles.push(prepared(owner.clone(), FULL));
                        objects.push(owner);
                        models[side].live.push(Entry {
                            handle,
                            object: id,
                            rights: FULL,
                        });
                    }
                    drop(reservation.publish(&mut tables[side], handles));
                }
                1 => {
                    let n = [1, 2, 31, 32, 63, 64][random.index(6)];
                    let reservation = crate::require_ok(tables[side].reserve_batch(n));
                    models[side].stale.extend_from_slice(reservation.values());
                    assert!(matches!(
                        tables[side].begin_teardown(),
                        Err(HandleError::OutstandingReservation)
                    ));
                    reservation.abort(&mut tables[side]);
                }
                2 => {
                    let index = random.index(models[side].live.len());
                    let entry = models[side].live.swap_remove(index);
                    crate::require_ok(tables[side].remove(entry.handle)).complete();
                    models[side].stale.push(entry.handle);
                }
                3 => {
                    let entry = models[side].live[random.index(models[side].live.len())];
                    let rights = if random.index(2) == 0 {
                        entry.rights
                    } else {
                        Rights::INSPECT
                    };
                    let duplicate = tables[side].duplicate(entry.handle, rights);
                    if entry.rights.contains(Rights::DUPLICATE) {
                        let [handle] = crate::require_ok(tables[side].reserve::<1>())
                            .publish(&mut tables[side], [crate::require_ok(duplicate)]);
                        models[side].live.push(Entry {
                            handle,
                            rights,
                            ..entry
                        });
                    } else {
                        assert_eq!(duplicate.err(), Some(HandleError::AccessDenied));
                    }
                }
                4 => {
                    let index = random.index(models[side].live.len());
                    let entry = models[side].live[index];
                    let rights = if random.index(2) == 0 {
                        entry.rights
                    } else {
                        Rights::INSPECT
                    };
                    let handle = crate::require_ok(tables[side].replace(entry.handle, rights));
                    models[side].stale.push(entry.handle);
                    models[side].live[index] = Entry {
                        handle,
                        rights,
                        ..entry
                    };
                }
                5 | 6 => {
                    let index = random.index(models[side].live.len());
                    let entry = models[side].live[index];
                    let request = HandleTransferRequest {
                        value: entry.handle,
                        offered_rights: None,
                        rights: entry.rights,
                        offered_kind: None,
                        expected_kind: Some(TEST_KIND),
                        operation: HandleTransferOperation::Move,
                    };
                    let claim = tables[side].prepare_transfer(
                        &[request],
                        None,
                        None,
                        HandleTransferRoute::Buffered,
                    );
                    if !entry.rights.contains(Rights::TRANSFER) {
                        assert_eq!(claim.err(), Some(HandleError::AccessDenied));
                    } else {
                        let claim = crate::require_ok(claim);
                        assert_eq!(
                            tables[side].remove(entry.handle).err(),
                            Some(HandleError::Busy)
                        );
                        assert!(matches!(
                            tables[side].begin_teardown(),
                            Err(HandleError::OutstandingReservation)
                        ));
                        if op == 5 {
                            claim.rollback(&mut tables[side]);
                        } else {
                            let handles = claim.commit(&mut tables[side]).into_prepared_handles();
                            let target = 1 - side;
                            let reservation = crate::require_ok(tables[target].reserve_batch(1));
                            let handle = reservation.values()[0];
                            drop(reservation.publish(&mut tables[target], handles));
                            models[side].live.swap_remove(index);
                            models[side].stale.push(entry.handle);
                            models[target].live.push(Entry { handle, ..entry });
                        }
                    }
                }
                7 => {
                    let entry = models[side].live[random.index(models[side].live.len())];
                    assert_eq!(
                        tables[side].duplicate(entry.handle, Rights::START).err(),
                        Some(HandleError::AccessDenied)
                    );
                    assert_eq!(
                        tables[side].replace(entry.handle, Rights::START),
                        Err(HandleError::AccessDenied)
                    );
                }
                8 => {
                    while let Some(page) = tables[side].take_empty_page() {
                        drop(page);
                    }
                }
                _ => {
                    // A valid first source followed by a stale source must not
                    // leave the first handle claimed after batch rejection.
                    let reservation = crate::require_ok(tables[side].reserve::<1>());
                    let stale = reservation.values()[0];
                    reservation.abort(&mut tables[side]);
                    models[side].stale.push(stale);
                    let entry = models[side].live[random.index(models[side].live.len())];
                    let requests = [entry.handle, stale].map(|value| HandleTransferRequest {
                        value,
                        offered_rights: None,
                        rights: Rights::INSPECT,
                        offered_kind: None,
                        expected_kind: Some(TEST_KIND),
                        operation: HandleTransferOperation::Move,
                    });
                    let error = tables[side]
                        .prepare_transfer(&requests, None, None, HandleTransferRoute::Buffered)
                        .err();
                    let expected = if entry.rights.contains(Rights::TRANSFER) {
                        HandleError::InvalidHandle
                    } else {
                        HandleError::AccessDenied
                    };
                    assert_eq!(error, Some(expected));
                }
            }
            for (table, model) in tables.iter().zip(&models) {
                check(table, model);
            }
            let mut counts = vec![0; objects.len()];
            for entry in models.iter().flat_map(|model| &model.live) {
                counts[entry.object] += 1;
            }
            for (owner, count) in objects.iter().zip(&counts) {
                assert_eq!(owner.active_handle_count(), *count);
            }
            assert_eq!(
                transitions.load(Ordering::Relaxed),
                counts.iter().filter(|count| **count == 0).count()
            );
        }
    }));
    // Retire even after an oracle failure, so a test panic cannot strand live
    // table authority. A production invariant abort is deliberately not caught.
    for table in &mut tables {
        remove_all(table);
        while let Some(page) = table.take_empty_page() {
            drop(page);
        }
    }
    if let Err(error) = result {
        eprintln!(
            "HANDLE_FUZZ_FAIL seed={seed} steps={steps}; tail (step, table, op, rng)={trace:?}"
        );
        std::panic::resume_unwind(error);
    }
    assert_eq!(transitions.load(Ordering::Relaxed), objects.len());
    drop(objects);
    let _ = kernel::object::reap_final_objects(usize::MAX);
}

fn setting(name: &str, default: usize, maximum: usize) -> usize {
    let value = match std::env::var(name) {
        Ok(value) => crate::require_ok(value.parse::<usize>()),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    };
    assert!(
        (1..=maximum).contains(&value),
        "{name} must be 1..={maximum}"
    );
    value
}

#[test]
fn seeded_handle_transactions() {
    let first = match std::env::var("HYPER_FUZZ_SEED") {
        Ok(seed) => crate::require_ok(seed.parse::<u64>()),
        Err(std::env::VarError::NotPresent) => 0,
        Err(error) => panic!("HYPER_FUZZ_SEED: {error}"),
    };
    let cases = setting("HYPER_FUZZ_CASES", 32, 1_000_000);
    let steps = setting("HYPER_FUZZ_STEPS", 256, 4096);
    for offset in 0..cases {
        let seed = first.wrapping_add(offset as u64);
        // Print before entering the kernel, preserving a reproducer even if an
        // invariant aborts the process instead of unwinding.
        crate::require_ok(writeln!(
            std::io::stderr().lock(),
            "HANDLE_FUZZ seed={seed} steps={steps}"
        ));
        run(seed, steps);
    }
}
