// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel object and process-local generation-handle transactions.

use core::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "capability_harness.rs"]
#[allow(dead_code, unused_imports)]
mod kernel;

use kernel::capability::{
    HandleBatchReservationStorage, HandleError, HandleFlags, HandleScanCursor, HandleTable,
    HandleTableStoragePlan, HandleTransferRequest, HandleTransferStorage, HandleValue,
    InTransitHandleBatch, PreparedHandle, Rights,
};
use kernel::object::{
    ExportPolicy, KernelObject, KernelRef, KernelService, ObjectHandleState, ObjectKind,
    ObjectRetirement, PublishableRef, Scheduler,
};

const TEST_KIND: ObjectKind = match NonZeroU32::new(0x7fff_ff01) {
    Some(value) => ObjectKind::for_test(value),
    None => panic!("test object kind must be nonzero"),
};
const OTHER_KIND: ObjectKind = match NonZeroU32::new(0x7fff_ff02) {
    Some(value) => ObjectKind::for_test(value),
    None => panic!("test object kind must be nonzero"),
};
const CASCADING_KIND: ObjectKind = match NonZeroU32::new(0x7fff_ff03) {
    Some(value) => ObjectKind::for_test(value),
    None => panic!("test object kind must be nonzero"),
};

struct TestObject {
    value: u64,
    zero_transitions: Arc<AtomicUsize>,
}

impl kernel::object::private::Sealed for TestObject {}
impl kernel::object::private::UserExportable for TestObject {}

impl KernelObject for TestObject {
    const KIND: ObjectKind = TEST_KIND;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::WAIT)
        .union(Rights::INSPECT);

    fn signal_source(&self) -> Option<kernel::object::signals::SignalSource<'_>> {
        Some(kernel::object::signals::SignalSource::for_test())
    }

    fn on_zero_active_handles(&self, _retirement: &mut ObjectRetirement) {
        self.zero_transitions.fetch_add(1, Ordering::Relaxed);
    }
}

struct CascadingObject {
    child: std::sync::Mutex<Option<InTransitHandleBatch>>,
    transitions: Arc<AtomicUsize>,
    callback_depth: Arc<AtomicUsize>,
    maximum_depth: Arc<AtomicUsize>,
}

impl kernel::object::private::Sealed for CascadingObject {}
impl kernel::object::private::UserExportable for CascadingObject {}

impl KernelObject for CascadingObject {
    const KIND: ObjectKind = CASCADING_KIND;
    const SUPPORTED_RIGHTS: Rights = Rights::TRANSFER.union(Rights::INSPECT);

    fn on_zero_active_handles(&self, retirement: &mut ObjectRetirement) {
        let depth = self.callback_depth.fetch_add(1, Ordering::Relaxed) + 1;
        self.maximum_depth.fetch_max(depth, Ordering::Relaxed);
        self.transitions.fetch_add(1, Ordering::Relaxed);
        let child = {
            let mut child = match self.child.lock() {
                Ok(child) => child,
                Err(poisoned) => poisoned.into_inner(),
            };
            child.take()
        };
        if let Some(child) = child {
            child.release_into(retirement);
        }
        self.callback_depth.fetch_sub(1, Ordering::Relaxed);
    }
}

struct OtherObject;

impl kernel::object::private::Sealed for OtherObject {}
impl kernel::object::private::UserExportable for OtherObject {}

impl KernelObject for OtherObject {
    const KIND: ObjectKind = OTHER_KIND;
    const SUPPORTED_RIGHTS: Rights = Rights::INSPECT;
}

struct VariantRightsObject {
    writable: bool,
}

impl kernel::object::private::Sealed for VariantRightsObject {}
impl kernel::object::private::UserExportable for VariantRightsObject {}

impl KernelObject for VariantRightsObject {
    const KIND: ObjectKind = OTHER_KIND;
    const SUPPORTED_RIGHTS: Rights = Rights::INSPECT.union(Rights::WRITE).union(Rights::EXECUTE);

    fn supported_rights(&self) -> Rights {
        if self.writable {
            Rights::INSPECT.union(Rights::WRITE)
        } else {
            Rights::INSPECT.union(Rights::EXECUTE)
        }
    }
}

struct DropTrackedObject(Arc<AtomicUsize>);

impl kernel::object::private::Sealed for DropTrackedObject {}

impl KernelObject for DropTrackedObject {
    const KIND: ObjectKind = OTHER_KIND;
    const SUPPORTED_RIGHTS: Rights = Rights::INSPECT;
}

impl Drop for DropTrackedObject {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

type TestObjectRef = PublishableRef<TestObject, KernelService>;

fn object(value: u64, zero_transitions: &Arc<AtomicUsize>) -> TestObjectRef {
    crate::require_ok(PublishableRef::try_new(TestObject {
        value,
        zero_transitions: zero_transitions.clone(),
    }))
}

fn prepared(object: TestObjectRef, rights: Rights) -> PreparedHandle {
    crate::require_ok(PreparedHandle::try_from_new_object(
        object.publication(),
        rights,
        HandleFlags::NONE,
    ))
}

fn cascading_handle(
    child: Option<PreparedHandle>,
    transitions: &Arc<AtomicUsize>,
    callback_depth: &Arc<AtomicUsize>,
    maximum_depth: &Arc<AtomicUsize>,
) -> PreparedHandle {
    let child = child.map(|handle| InTransitHandleBatch::from_prepared_handles(vec![handle]));
    let object = crate::require_ok(PublishableRef::try_new(CascadingObject {
        child: std::sync::Mutex::new(child),
        transitions: transitions.clone(),
        callback_depth: callback_depth.clone(),
        maximum_depth: maximum_depth.clone(),
    }));
    crate::require_ok(PreparedHandle::try_from_new_object(
        object.publication(),
        Rights::TRANSFER.union(Rights::INSPECT),
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

#[test]
fn retirement_worklist_flattens_nested_final_handle_callbacks() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let callback_depth = Arc::new(AtomicUsize::new(0));
    let maximum_depth = Arc::new(AtomicUsize::new(0));
    let mut retirement = ObjectRetirement::new();

    for _ in 0..2 {
        let child = cascading_handle(None, &transitions, &callback_depth, &maximum_depth);
        let parent = cascading_handle(Some(child), &transitions, &callback_depth, &maximum_depth);
        InTransitHandleBatch::from_prepared_handles(vec![parent]).release_into(&mut retirement);
        retirement.drain();
        assert_eq!(callback_depth.load(Ordering::Relaxed), 0);
    }

    assert_eq!(transitions.load(Ordering::Relaxed), 4);
    assert_eq!(maximum_depth.load(Ordering::Relaxed), 1);
    drop(retirement);
}

#[test]
fn zero_reference_object_drop_is_deferred_and_reaped_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let object: KernelRef<DropTrackedObject, Scheduler> = crate::require_ok(
        KernelRef::try_new_scheduler(DropTrackedObject(drops.clone())),
    );

    drop(object);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert!(kernel::object::final_reap_pending());

    let mut attempts = 0usize;
    while drops.load(Ordering::Acquire) == 0 && attempts < 4_096 {
        // Host tests share the global queue and run concurrently, so another
        // test may consume the next item between observing this probe and the
        // reap attempt. Only this probe's exactly-once drop is authoritative.
        let _ = kernel::object::reap_one_final_object();
        std::thread::yield_now();
        attempts += 1;
    }
    assert_eq!(drops.load(Ordering::Acquire), 1);

    // Draining work queued by this test cannot destroy the target twice.
    let _ = kernel::object::reap_final_objects(4_096);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn object_snapshot_distinguishes_unpublished_active_and_retired_authority() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(9, &transitions);
    let unpublished = object.snapshot();
    assert_eq!(unpublished.kind, TEST_KIND);
    assert_eq!(unpublished.handles, ObjectHandleState::Unpublished);
    assert_eq!(unpublished.export_policy, ExportPolicy::User);
    assert_eq!(unpublished.references.kernel_service, 1);

    let publication = object.publication();
    let pending = object.snapshot();
    assert_eq!(pending.references.publication, 1);
    assert_eq!(pending.references.user_authority, 0);
    let handle = crate::require_ok(PreparedHandle::try_from_new_object(
        publication,
        Rights::INSPECT,
        HandleFlags::NONE,
    ));
    let active = object.snapshot();
    assert_eq!(active.handles, ObjectHandleState::Active(1));
    assert_eq!(active.references.kernel_service, 1);
    assert_eq!(active.references.publication, 0);
    assert_eq!(active.references.user_authority, 1);
    drop(handle);
    let retired = object.snapshot();
    assert_eq!(retired.handles, ObjectHandleState::Retired);
    assert_eq!(retired.references.user_authority, 0);
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn handle_diagnostics_page_generation_qualified_object_edges() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let mut table = HandleTable::new();
    let reservation = crate::require_ok(table.reserve_batch(40));
    let mut expected = reservation.values().to_vec();
    let handles = (0_u64..40)
        .map(|value| prepared(object(value, &transitions), Rights::INSPECT))
        .collect();
    reservation.publish(&mut table, handles);

    let first = crate::require_ok(table.scan_handles(HandleScanCursor::start()));
    let mut observed: Vec<_> = first.entries().map(|entry| entry.value).collect();
    let next = match first.next() {
        Some(cursor) => cursor,
        None => {
            remove_all(&mut table);
            panic!("forty handles require a second diagnostic page");
        }
    };
    let second = crate::require_ok(table.scan_handles(next));
    observed.extend(second.entries().map(|entry| entry.value));
    let completed = second.next().is_none();

    remove_all(&mut table);
    expected.sort_by_key(|value| value.get());
    observed.sort_by_key(|value| value.get());
    assert_eq!(observed, expected);
    assert!(completed);
    assert_eq!(transitions.load(Ordering::Relaxed), 40);
}

#[test]
fn reservation_values_are_unresolvable_until_one_final_publication() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(17, &transitions);
    let mut table = HandleTable::new();
    let reservation = crate::require_ok(table.reserve::<2>());
    let values = reservation.values();
    let second_transitions = Arc::new(AtomicUsize::new(0));
    let second_object = object(18, &second_transitions);
    let handles = [
        prepared(first_object.clone(), Rights::INSPECT),
        prepared(second_object, Rights::INSPECT),
    ];
    let published = reservation.publish(&mut table, handles);

    assert_eq!(published, values);
    assert_eq!(first_object.active_handle_count(), 1);
    assert_eq!(
        crate::require_ok(table.get_info(values[0])).koid,
        first_object.koid()
    );
    assert_eq!(crate::require_ok(table.get_info(values[1])).kind, TEST_KIND);
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn aborted_reservations_invalidate_every_exposed_future_value() {
    let mut table = HandleTable::new();
    assert!(table.free_list_is_consistent_for_test());
    let first_values = {
        let reservation = crate::require_ok(table.reserve::<2>());
        let values = reservation.values();
        reservation.abort(&mut table);
        values
    };
    assert!(table.free_list_is_consistent_for_test());
    assert_eq!(
        table.get_info(first_values[0]),
        Err(HandleError::InvalidHandle)
    );

    let second_values = {
        let reservation = crate::require_ok(table.reserve::<2>());
        let values = reservation.values();
        reservation.abort(&mut table);
        values
    };
    assert!(table.free_list_is_consistent_for_test());
    assert_ne!(second_values, first_values);
    assert_eq!(
        table.get_info(first_values[1]),
        Err(HandleError::InvalidHandle)
    );
}

#[test]
fn runtime_batch_reservation_publishes_exact_handle_count() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(71, &transitions);
    let second_object = object(72, &transitions);
    let mut table = HandleTable::new();
    let reservation = crate::require_ok(table.reserve_batch(2));
    let future = reservation.values().to_vec();
    let handles = vec![
        prepared(first_object, Rights::INSPECT),
        prepared(second_object, Rights::WAIT),
    ];
    let retired = reservation.publish(&mut table, handles);

    // Publication moves every owner into the table but retains the allocation
    // itself for destruction by the Process after its serialization lock exits.
    assert!(retired.retained_handle_capacity_for_test() >= 2);
    drop(retired);

    assert_eq!(
        crate::require_ok(table.get_info(future[0])).rights,
        Rights::INSPECT
    );
    assert_eq!(
        crate::require_ok(table.get_info(future[1])).rights,
        Rights::WAIT
    );
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 2);
}

#[test]
fn runtime_batch_storage_rejects_unbounded_lock_work() {
    assert!(matches!(
        HandleBatchReservationStorage::try_new(0),
        Err(HandleError::EmptyReservation)
    ));
    assert!(matches!(
        HandleBatchReservationStorage::try_new(65),
        Err(HandleError::ReservationTooLarge)
    ));
    assert!(matches!(
        HandleTransferStorage::try_new(0),
        Err(HandleError::EmptyReservation)
    ));
    assert!(matches!(
        HandleTransferStorage::try_new(65),
        Err(HandleError::ReservationTooLarge)
    ));
}

#[test]
fn stale_segment_plan_is_discarded_before_table_mutation() {
    let mut table = HandleTable::new();
    let snapshot = crate::require_ok(table.reservation_storage_snapshot_for(1));
    let plan = crate::require_ok(HandleTableStoragePlan::try_new(snapshot));

    let intervening = crate::require_ok(table.reserve::<1>());
    intervening.abort(&mut table);

    let current = crate::require_ok(table.reservation_storage_snapshot_for(1));
    assert_ne!(current, plan.snapshot());
    drop(plan);
    assert!(table.free_list_is_consistent_for_test());
    remove_all(&mut table);
}

#[test]
fn segment_growth_failure_leaves_existing_table_state_unchanged() {
    let mut table = HandleTable::new();
    let before = crate::require_ok(table.reservation_storage_snapshot_for(1));

    assert_eq!(
        HandleTableStoragePlan::force_allocation_failure_for_test(),
        Err(HandleError::Allocation)
    );

    assert_eq!(
        crate::require_ok(table.reservation_storage_snapshot_for(1)),
        before
    );
    let reservation = crate::require_ok(table.reserve::<1>());
    reservation.abort(&mut table);
    remove_all(&mut table);
}

#[test]
fn segment_install_preserves_existing_numeric_handles() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let mut table = HandleTable::new();
    let first_segment = crate::require_ok(table.reserve_batch(64));
    let values = first_segment.values().to_vec();
    let handles = (0_u64..64)
        .map(|value| prepared(object(value, &transitions), Rights::INSPECT))
        .collect();
    first_segment.publish(&mut table, handles);
    let stable = values[0];

    let snapshot = crate::require_ok(table.reservation_storage_snapshot_for(1));
    let mut plan = Some(crate::require_ok(HandleTableStoragePlan::try_new(snapshot)));
    let reservation = crate::require_ok(table.reserve_with_plan::<1>(&mut plan));
    let replacement = prepared(object(64, &transitions), Rights::WAIT);
    let published = reservation.publish(&mut table, [replacement]);

    assert_eq!(
        crate::require_ok(table.get_info(stable)).rights,
        Rights::INSPECT
    );
    assert_eq!(
        crate::require_ok(table.get_info(published[0])).rights,
        Rights::WAIT
    );
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 65);
}

#[test]
fn untrusted_handle_values_are_validated_before_slot_arithmetic() {
    assert_eq!(
        HandleValue::try_from_raw(0),
        Err(HandleError::InvalidHandle)
    );
    assert_eq!(
        HandleValue::try_from_raw(1),
        Err(HandleError::InvalidHandle)
    );
    assert_eq!(
        HandleValue::try_from_raw(1 << 24),
        Err(HandleError::InvalidHandle)
    );
    assert_eq!(
        crate::require_ok(HandleValue::try_from_raw((1 << 24) | 1)).get(),
        (1 << 24) | 1
    );
}

#[test]
fn closing_a_handle_invalidates_its_generation_before_slot_reuse() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(1, &transitions);
    let mut table = HandleTable::new();
    let first = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(first_object, Rights::INSPECT)])[0]
    };
    let closed = crate::require_ok(table.remove(first));
    assert!(table.free_list_is_consistent_for_test());
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    closed.complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
    assert_eq!(table.get_info(first), Err(HandleError::InvalidHandle));

    let second_object = object(2, &transitions);
    let second = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(second_object, Rights::INSPECT)])[0]
    };
    assert!(table.free_list_is_consistent_for_test());
    assert_ne!(second, first);
    assert_eq!(table.get_info(first), Err(HandleError::InvalidHandle));
    assert_eq!(
        crate::require_ok(table.get_info(second)).rights,
        Rights::INSPECT
    );
    crate::require_ok(table.remove(second)).complete();
    assert!(table.free_list_is_consistent_for_test());
}

#[test]
fn duplicate_can_only_attenuate_existing_authority() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(7, &transitions);
    let source_rights = Rights::DUPLICATE.union(Rights::INSPECT);
    let mut table = HandleTable::new();
    let source = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(object.clone(), source_rights)])[0]
    };

    assert_eq!(
        table.duplicate(source, Rights::WAIT).err(),
        Some(HandleError::AccessDenied)
    );
    let duplicate = crate::require_ok(table.duplicate(source, Rights::INSPECT));
    assert_eq!(object.active_handle_count(), 2);
    let duplicate_value = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [duplicate])[0]
    };
    assert_eq!(
        crate::require_ok(table.get_info(duplicate_value)).rights,
        Rights::INSPECT
    );
    assert_eq!(
        table.duplicate(duplicate_value, Rights::INSPECT).err(),
        Some(HandleError::AccessDenied)
    );
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn replace_moves_active_authority_in_one_infallible_commit() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(8, &transitions);
    let source_rights = Rights::DUPLICATE.union(Rights::WAIT).union(Rights::INSPECT);
    let mut table = HandleTable::new();
    let source = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(object.clone(), source_rights)])[0]
    };

    assert_eq!(
        table.replace(source, Rights::TRANSFER),
        Err(HandleError::AccessDenied)
    );
    assert_eq!(
        crate::require_ok(table.get_info(source)).rights,
        source_rights
    );
    let replacement = crate::require_ok(table.replace(source, Rights::WAIT));
    assert_ne!(replacement, source);
    assert_eq!(table.get_info(source), Err(HandleError::InvalidHandle));
    assert_eq!(
        crate::require_ok(table.get_info(replacement)).rights,
        Rights::WAIT
    );
    assert_eq!(object.active_handle_count(), 1);
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    crate::require_ok(table.remove(replacement)).complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn replace_retires_an_exhausted_generation_without_losing_authority() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(81, &transitions);
    let mut table = HandleTable::new();
    let source = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(object.clone(), Rights::INSPECT)])[0]
    };
    let exhausted =
        table.set_occupied_generation_for_test(source, HandleTable::maximum_generation_for_test());

    let replacement = crate::require_ok(table.replace(exhausted, Rights::INSPECT));

    assert_ne!(replacement, exhausted);
    assert_eq!(table.get_info(exhausted), Err(HandleError::InvalidHandle));
    assert_eq!(object.active_handle_count(), 1);
    assert!(table.free_list_is_consistent_for_test());
    crate::require_ok(table.remove(replacement)).complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn unknown_abi_right_bits_are_rejected() {
    assert_eq!(Rights::from_bits(1 << 63), None);
    assert_eq!(
        Rights::from_bits(Rights::DUPLICATE.bits()),
        Some(Rights::DUPLICATE)
    );
    assert!(!Rights::DUPLICATE.is_empty());
}

#[test]
fn typed_resolution_uses_compiler_checked_downcast_and_survives_close() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(99, &transitions);
    let koid = object.koid();
    let mut table = HandleTable::new();
    let value = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(object.clone(), Rights::INSPECT)])[0]
    };

    assert!(matches!(
        table.resolve::<OtherObject>(value, Rights::INSPECT),
        Err(HandleError::WrongObjectType)
    ));
    assert!(matches!(
        table.resolve::<TestObject>(value, Rights::WAIT),
        Err(HandleError::AccessDenied)
    ));
    let resolved = crate::require_ok(table.resolve::<TestObject>(value, Rights::INSPECT));
    assert_eq!(resolved.koid(), koid);
    let pinned = object.snapshot();
    assert_eq!(pinned.references.kernel_service, 1);
    assert_eq!(pinned.references.user_authority, 1);
    assert_eq!(pinned.references.operation_pin, 1);
    let closed = crate::require_ok(table.remove(value));
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    closed.complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
    let retired = object.snapshot();
    assert_eq!(retired.references.user_authority, 0);
    assert_eq!(retired.references.operation_pin, 1);
    assert_eq!(resolved.object().value, 99);
}

#[test]
fn scheduler_ownership_is_counted_separately_from_service_ownership() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let service = object(102, &transitions);
    let scheduler = service.clone().into_scheduler();

    let snapshot = service.snapshot();
    assert_eq!(snapshot.references.kernel_service, 1);
    assert_eq!(snapshot.references.scheduler, 1);
    assert_eq!(snapshot.references.user_authority, 0);

    drop(scheduler);
    assert_eq!(service.snapshot().references.scheduler, 0);
}

#[test]
fn concurrent_close_retires_user_authority_without_revoking_an_operation_pin() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(101, &transitions);
    let mut table = HandleTable::new();
    let value = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(&mut table, [prepared(object, Rights::INSPECT)])[0]
    };
    let resolved = crate::require_ok(table.resolve::<TestObject>(value, Rights::INSPECT));
    let rendezvous = Arc::new(std::sync::Barrier::new(2));
    let operation_rendezvous = rendezvous.clone();
    let operation = std::thread::spawn(move || {
        operation_rendezvous.wait();
        operation_rendezvous.wait();
        resolved.object().value
    });

    rendezvous.wait();
    crate::require_ok(table.remove(value)).complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
    rendezvous.wait();

    let observed = match operation.join() {
        Ok(observed) => observed,
        Err(_) => panic!("resolved operation thread panicked"),
    };
    assert_eq!(observed, 101);
}

#[test]
fn waitable_resolution_uses_the_erased_object_capability() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(100, &transitions);
    let koid = object.koid();
    let mut table = HandleTable::new();
    let value = {
        let reservation = crate::require_ok(table.reserve::<1>());
        reservation.publish(
            &mut table,
            [prepared(object, Rights::WAIT.union(Rights::INSPECT))],
        )[0]
    };

    let resolved = crate::require_ok(table.resolve_waitable(value, Rights::WAIT));
    assert_eq!(resolved.koid(), koid);
    let _source = resolved.source();

    crate::require_ok(table.remove(value)).complete();
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn unpublished_active_handle_rollback_runs_the_zero_transition_once() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(4, &transitions);
    let handle = prepared(object.clone(), Rights::INSPECT);
    assert_eq!(object.active_handle_count(), 1);
    assert_eq!(
        PreparedHandle::try_from_new_object(
            object.publication(),
            Rights::INSPECT,
            HandleFlags::NONE,
        )
        .err(),
        Some(HandleError::ObjectAlreadyActive)
    );
    drop(handle);

    assert!(object.is_retired());
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
    assert_eq!(
        PreparedHandle::try_from_new_object(
            object.publication(),
            Rights::INSPECT,
            HandleFlags::NONE,
        )
        .err(),
        Some(HandleError::ObjectRetired)
    );
}

#[test]
fn concurrent_active_handle_owners_publish_one_zero_transition() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let object = object(12, &transitions);
    let anchor = Arc::new(prepared(object.clone(), Rights::INSPECT));
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let anchor = anchor.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let handle = crate::require_ok(anchor.duplicate_for_test(Rights::INSPECT));
            barrier.wait();
            drop(handle);
        }));
    }

    barrier.wait();
    for worker in workers {
        assert!(worker.join().is_ok());
    }
    assert_eq!(object.active_handle_count(), 1);
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    drop(anchor);
    assert!(object.is_retired());
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn teardown_cursor_blocks_publication_and_scans_each_slot_once() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(20, &transitions);
    let second_transitions = Arc::new(AtomicUsize::new(0));
    let second_object = object(21, &second_transitions);
    let mut table = HandleTable::new();
    let values = {
        let reservation = crate::require_ok(table.reserve::<2>());
        reservation.publish(
            &mut table,
            [
                prepared(first_object, Rights::INSPECT),
                prepared(second_object, Rights::INSPECT),
            ],
        )
    };

    let mut cursor = crate::require_ok(table.begin_teardown());
    assert_eq!(table.get_info(values[0]), Err(HandleError::TableRetired));
    assert!(matches!(
        table.reserve::<1>(),
        Err(HandleError::TableRetired)
    ));
    let first = crate::require_some(table.remove_next(&mut cursor));
    first.complete();
    let second = crate::require_some(table.remove_next(&mut cursor));
    second.complete();
    assert!(table.remove_next(&mut cursor).is_none());
    table.finish_teardown(cursor);
    assert_eq!(transitions.load(Ordering::Relaxed), 1);
}

#[test]
fn teardown_waits_for_detached_slot_reservations() {
    let mut table = HandleTable::new();
    let reservation = crate::require_ok(table.reserve::<1>());
    let future = reservation.values()[0];

    assert!(matches!(
        table.begin_teardown(),
        Err(HandleError::OutstandingReservation)
    ));
    assert_eq!(table.get_info(future), Err(HandleError::InvalidHandle));
    reservation.abort(&mut table);

    let cursor = crate::require_ok(table.begin_teardown());
    table.finish_teardown(cursor);
}

#[test]
fn transfer_claim_rollback_restores_exact_values_and_authority() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(31, &transitions);
    let second_object = object(32, &transitions);
    let rights = Rights::TRANSFER.union(Rights::INSPECT);
    let mut table = HandleTable::new();
    let values = {
        let reservation = crate::require_ok(table.reserve::<2>());
        reservation.publish(
            &mut table,
            [
                prepared(first_object.clone(), rights),
                prepared(second_object.clone(), rights),
            ],
        )
    };
    let requests = [
        HandleTransferRequest {
            value: values[0],
            rights: Rights::INSPECT,
            expected_kind: None,
        },
        HandleTransferRequest {
            value: values[1],
            rights,
            expected_kind: None,
        },
    ];

    let claim = crate::require_ok(table.prepare_transfer(&requests, None, None));
    assert_eq!(table.get_info(values[0]), Err(HandleError::Busy));
    assert_eq!(table.remove(values[1]).err(), Some(HandleError::Busy));
    assert!(matches!(
        table.begin_teardown(),
        Err(HandleError::OutstandingReservation)
    ));
    assert_eq!(first_object.active_handle_count(), 1);
    assert_eq!(second_object.active_handle_count(), 1);

    claim.rollback(&mut table);
    assert_eq!(crate::require_ok(table.get_info(values[0])).rights, rights);
    assert_eq!(crate::require_ok(table.get_info(values[1])).rights, rights);
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 2);
}

#[test]
fn transfer_commit_moves_active_owners_and_advances_source_generations() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first_object = object(41, &transitions);
    let second_object = object(42, &transitions);
    let rights = Rights::TRANSFER.union(Rights::WAIT).union(Rights::INSPECT);
    let mut source_table = HandleTable::new();
    let source_values = {
        let reservation = crate::require_ok(source_table.reserve::<2>());
        reservation.publish(
            &mut source_table,
            [
                prepared(first_object.clone(), rights),
                prepared(second_object.clone(), rights),
            ],
        )
    };
    let requests = [
        HandleTransferRequest {
            value: source_values[0],
            rights: Rights::WAIT,
            expected_kind: None,
        },
        HandleTransferRequest {
            value: source_values[1],
            rights: Rights::INSPECT,
            expected_kind: None,
        },
    ];

    let claim = crate::require_ok(source_table.prepare_transfer(&requests, None, None));
    let batch = claim.commit(&mut source_table);
    assert_eq!(batch.len(), 2);
    assert_eq!(
        source_table.get_info(source_values[0]),
        Err(HandleError::InvalidHandle)
    );
    assert_eq!(
        source_table.get_info(source_values[1]),
        Err(HandleError::InvalidHandle)
    );
    assert_eq!(first_object.active_handle_count(), 1);
    assert_eq!(second_object.active_handle_count(), 1);
    let mut handles = batch.into_prepared_handles().into_iter();
    let first = crate::require_some(handles.next());
    let second = crate::require_some(handles.next());
    assert!(handles.next().is_none());

    let mut destination_table = HandleTable::new();
    let destination_values = {
        let reservation = crate::require_ok(destination_table.reserve::<2>());
        reservation.publish(&mut destination_table, [first, second])
    };
    assert_eq!(
        crate::require_ok(destination_table.get_info(destination_values[0])).rights,
        Rights::WAIT
    );
    assert_eq!(
        crate::require_ok(destination_table.get_info(destination_values[1])).rights,
        Rights::INSPECT
    );
    assert_eq!(transitions.load(Ordering::Relaxed), 0);
    remove_all(&mut source_table);
    remove_all(&mut destination_table);
    assert_eq!(transitions.load(Ordering::Relaxed), 2);
}

#[test]
fn transfer_validation_is_all_or_nothing() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let transferable = object(51, &transitions);
    let immovable = object(52, &transitions);
    let rights = Rights::TRANSFER.union(Rights::INSPECT);
    let mut table = HandleTable::new();
    let values = {
        let reservation = crate::require_ok(table.reserve::<2>());
        reservation.publish(
            &mut table,
            [
                prepared(transferable.clone(), rights),
                prepared(immovable, Rights::INSPECT),
            ],
        )
    };

    let duplicate = [
        HandleTransferRequest {
            value: values[0],
            rights: Rights::INSPECT,
            expected_kind: None,
        },
        HandleTransferRequest {
            value: values[0],
            rights: Rights::INSPECT,
            expected_kind: None,
        },
    ];
    assert_eq!(
        table.prepare_transfer(&duplicate, None, None).err(),
        Some(HandleError::InvalidHandle)
    );
    let excessive = [HandleTransferRequest {
        value: values[0],
        rights: Rights::WAIT,
        expected_kind: None,
    }];
    assert_eq!(
        table.prepare_transfer(&excessive, None, None).err(),
        Some(HandleError::AccessDenied)
    );
    let missing_transfer = [HandleTransferRequest {
        value: values[1],
        rights: Rights::INSPECT,
        expected_kind: None,
    }];
    assert_eq!(
        table.prepare_transfer(&missing_transfer, None, None).err(),
        Some(HandleError::AccessDenied)
    );
    let forbidden = [HandleTransferRequest {
        value: values[0],
        rights: Rights::INSPECT,
        expected_kind: None,
    }];
    assert_eq!(
        table
            .prepare_transfer(&forbidden, Some(transferable.koid()), None)
            .err(),
        Some(HandleError::AccessDenied)
    );
    let wrong_kind = [HandleTransferRequest {
        value: values[0],
        rights: Rights::INSPECT,
        expected_kind: Some(OTHER_KIND),
    }];
    assert_eq!(
        table.prepare_transfer(&wrong_kind, None, None).err(),
        Some(HandleError::WrongObjectType)
    );
    assert_eq!(
        table
            .prepare_transfer(&forbidden, None, Some(TEST_KIND))
            .err(),
        Some(HandleError::UnsupportedTransfer)
    );
    let disguised_forbidden = [HandleTransferRequest {
        value: values[0],
        rights: Rights::INSPECT,
        expected_kind: Some(OTHER_KIND),
    }];
    assert_eq!(
        table
            .prepare_transfer(
                &disguised_forbidden,
                Some(transferable.koid()),
                Some(TEST_KIND),
            )
            .err(),
        Some(HandleError::UnsupportedTransfer)
    );

    assert_eq!(crate::require_ok(table.get_info(values[0])).rights, rights);
    assert_eq!(
        crate::require_ok(table.get_info(values[1])).rights,
        Rights::INSPECT
    );
    assert_eq!(transferable.active_handle_count(), 1);
    remove_all(&mut table);
    assert_eq!(transitions.load(Ordering::Relaxed), 2);
}

#[test]
fn koids_are_unique_but_do_not_participate_in_lookup() {
    let transitions = Arc::new(AtomicUsize::new(0));
    let first = object(1, &transitions);
    let second = object(2, &transitions);

    assert_ne!(first.koid(), second.koid());
    assert_ne!(first.koid().get(), 0);
    assert_eq!(first.snapshot().kind.get(), TEST_KIND.get());
}

#[test]
fn instance_rights_narrow_a_sum_type_without_cross_variant_authority() {
    let writable = crate::require_ok(PublishableRef::try_new(VariantRightsObject {
        writable: true,
    }));
    assert_eq!(
        writable.snapshot().supported_rights,
        Rights::INSPECT.union(Rights::WRITE)
    );
    assert_eq!(
        PreparedHandle::try_from_new_object(
            writable.publication(),
            Rights::EXECUTE,
            HandleFlags::NONE,
        )
        .err(),
        Some(HandleError::UnsupportedRights)
    );

    let executable = crate::require_ok(PublishableRef::try_new(VariantRightsObject {
        writable: false,
    }));
    assert_eq!(
        executable.snapshot().supported_rights,
        Rights::INSPECT.union(Rights::EXECUTE)
    );
    assert_eq!(
        PreparedHandle::try_from_new_object(
            executable.publication(),
            Rights::WRITE,
            HandleFlags::NONE,
        )
        .err(),
        Some(HandleError::UnsupportedRights)
    );
}
