// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Hierarchical resource-domain accounting and owned charge transactions.

use std::sync::{Arc, Barrier};

#[path = "../../../../src/kernel/accounting/mod.rs"]
#[allow(dead_code, unused_imports)]
mod accounting;

use accounting::{ResourceAmount, ResourceDomain, ResourceError, ResourceKind, ResourceLimits};

fn amount(kind: ResourceKind, value: u64) -> ResourceAmount {
    ResourceAmount::ZERO.with(kind, value)
}

fn all_kinds() -> [ResourceKind; 19] {
    [
        ResourceKind::KernelMemoryBytes,
        ResourceKind::Processes,
        ResourceKind::Threads,
        ResourceKind::Handles,
        ResourceKind::KernelObjects,
        ResourceKind::CommittedPages,
        ResourceKind::PinnedPages,
        ResourceKind::GuestPages,
        ResourceKind::IpcMessages,
        ResourceKind::IpcBytes,
        ResourceKind::IpcHandles,
        ResourceKind::Subscriptions,
        ResourceKind::Timers,
        ResourceKind::VirtualMachines,
        ResourceKind::VirtualCpus,
        ResourceKind::DeviceLeases,
        ResourceKind::DmaMappings,
        ResourceKind::UserAddressSpaces,
        ResourceKind::UserMappings,
    ]
}

#[test]
fn reservation_commit_and_release_preserve_distinct_usage_states() {
    let domain = crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Handles, 4),
    ));
    let request = amount(ResourceKind::Handles, 2);

    let reservation = crate::require_ok(domain.reserve(request));
    assert_eq!(reservation.domain_id(), domain.id());
    assert_eq!(reservation.amount(), request);
    assert_eq!(domain.usage().pending(ResourceKind::Handles), 2);
    assert_eq!(domain.usage().committed(ResourceKind::Handles), 0);

    let committed = reservation.commit();
    assert_eq!(committed.domain_id(), domain.id());
    assert_eq!(committed.amount(), request);
    assert_eq!(domain.usage().pending(ResourceKind::Handles), 0);
    assert_eq!(domain.usage().committed(ResourceKind::Handles), 2);

    drop(committed);
    assert_eq!(domain.usage().total(ResourceKind::Handles), 0);
}

#[test]
fn explicit_abort_and_drop_each_roll_back_pending_usage_once() {
    let root = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let child = crate::require_ok(root.try_new_child(ResourceLimits::UNLIMITED));
    let request = amount(ResourceKind::Timers, 3);

    crate::require_ok(child.reserve(request)).abort();
    assert_eq!(root.usage().total(ResourceKind::Timers), 0);
    assert_eq!(child.usage().total(ResourceKind::Timers), 0);

    {
        let _rollback_on_drop = crate::require_ok(child.reserve(request));
        assert_eq!(root.usage().pending(ResourceKind::Timers), 3);
        assert_eq!(child.usage().pending(ResourceKind::Timers), 3);
    }
    assert_eq!(root.usage().total(ResourceKind::Timers), 0);
    assert_eq!(child.usage().total(ResourceKind::Timers), 0);
}

#[test]
fn a_failed_leaf_dimension_restores_every_ancestor_and_dimension() {
    let root = crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED
            .with(ResourceKind::Handles, 10)
            .with(ResourceKind::Threads, 10),
    ));
    let child = crate::require_ok(
        root.try_new_child(
            ResourceLimits::UNLIMITED
                .with(ResourceKind::Handles, 10)
                .with(ResourceKind::Threads, 1),
        ),
    );
    let request = ResourceAmount::ZERO
        .with(ResourceKind::Handles, 4)
        .with(ResourceKind::Threads, 2);

    assert!(matches!(
        child.reserve(request),
        Err(ResourceError::LimitExceeded {
            domain,
            resource: ResourceKind::Threads,
            ..
        }) if domain == child.id()
    ));
    for kind in [ResourceKind::Handles, ResourceKind::Threads] {
        assert_eq!(root.usage().total(kind), 0);
        assert_eq!(child.usage().total(kind), 0);
    }
}

#[test]
fn empty_requests_are_rejected_without_observable_usage() {
    let domain = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    assert!(matches!(
        domain.reserve(ResourceAmount::ZERO),
        Err(ResourceError::EmptyCharge)
    ));
    for kind in all_kinds() {
        assert_eq!(domain.usage().total(kind), 0);
    }
}

#[test]
fn every_resource_kind_has_an_independent_counter_and_limit() {
    let kinds = all_kinds();
    for (index, limited) in kinds.iter().copied().enumerate() {
        let domain = crate::require_ok(ResourceDomain::try_new_root(
            ResourceLimits::UNLIMITED.with(limited, 0),
        ));
        assert!(matches!(
            domain.reserve(amount(limited, 1)),
            Err(ResourceError::LimitExceeded { resource, .. }) if resource == limited
        ));

        let independent = kinds[(index + 1) % kinds.len()];
        let reservation = crate::require_ok(domain.reserve(amount(independent, 1)));
        assert_eq!(domain.usage().total(limited), 0);
        assert_eq!(domain.usage().total(independent), 1);
        reservation.abort();
    }
}

#[test]
fn limit_changes_account_for_both_pending_and_committed_usage() {
    let domain = crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 100),
    ));
    let reservation = crate::require_ok(domain.reserve(amount(ResourceKind::CommittedPages, 60)));

    assert!(matches!(
        domain.set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 59)),
        Err(ResourceError::LimitBelowUsage {
            resource: ResourceKind::CommittedPages,
            used: 60,
            ..
        })
    ));
    assert_eq!(domain.local_limits().get(ResourceKind::CommittedPages), 100);

    let committed = reservation.commit();
    assert!(
        domain
            .set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 60))
            .is_ok()
    );
    assert!(matches!(
        domain.set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 59)),
        Err(ResourceError::LimitBelowUsage { .. })
    ));
    drop(committed);
    assert!(
        domain
            .set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 0))
            .is_ok()
    );
}

#[test]
fn maximum_usage_reports_overflow_without_aliasing_other_dimensions() {
    let domain = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let maximum = crate::require_ok(domain.reserve(amount(ResourceKind::Handles, u64::MAX)));

    assert!(matches!(
        domain.reserve(amount(ResourceKind::Handles, 1)),
        Err(ResourceError::UsageOverflow {
            resource: ResourceKind::Handles,
            ..
        })
    ));
    let mapping = crate::require_ok(domain.reserve(amount(ResourceKind::UserMappings, 1)));
    assert_eq!(domain.usage().pending(ResourceKind::Handles), u64::MAX);
    assert_eq!(domain.usage().pending(ResourceKind::UserMappings), 1);

    mapping.abort();
    maximum.abort();
    assert_eq!(domain.usage().total(ResourceKind::Handles), 0);
    assert_eq!(domain.usage().total(ResourceKind::UserMappings), 0);
}

#[test]
fn sibling_reservations_cannot_overbook_the_shared_ancestor() {
    let root = crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Threads, 1),
    ));
    let first = crate::require_ok(root.try_new_child(ResourceLimits::UNLIMITED));
    let second = crate::require_ok(root.try_new_child(ResourceLimits::UNLIMITED));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for domain in [first, second] {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            domain.reserve(amount(ResourceKind::Threads, 1))
        }));
    }

    barrier.wait();
    let mut successes = 0;
    let mut reservations = Vec::new();
    for worker in workers {
        match crate::require_ok(worker.join()) {
            Ok(reservation) => {
                successes += 1;
                reservations.push(reservation);
            }
            Err(ResourceError::LimitExceeded {
                domain,
                resource: ResourceKind::Threads,
                ..
            }) => assert_eq!(domain, root.id()),
            Err(error) => panic!("unexpected reservation error: {error:?}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(root.usage().pending(ResourceKind::Threads), 1);
    drop(reservations);
    assert_eq!(root.usage().total(ResourceKind::Threads), 0);
}

#[test]
fn limit_reduction_and_reservation_have_one_lock_ordered_outcome() {
    for _ in 0..64 {
        let domain = crate::require_ok(ResourceDomain::try_new_root(
            ResourceLimits::UNLIMITED.with(ResourceKind::Threads, 10),
        ));
        let barrier = Arc::new(Barrier::new(3));
        let reserve_domain = domain.clone();
        let reserve_barrier = barrier.clone();
        let reserve = std::thread::spawn(move || {
            reserve_barrier.wait();
            reserve_domain.reserve(amount(ResourceKind::Threads, 7))
        });
        let limit_domain = domain.clone();
        let limit_barrier = barrier.clone();
        let reduce = std::thread::spawn(move || {
            limit_barrier.wait();
            limit_domain.set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::Threads, 5))
        });

        barrier.wait();
        let reserve_result = crate::require_ok(reserve.join());
        let limit_result = crate::require_ok(reduce.join());
        match (reserve_result, limit_result) {
            (Ok(reservation), Err(ResourceError::LimitBelowUsage { used: 7, .. })) => {
                assert_eq!(domain.usage().pending(ResourceKind::Threads), 7);
                reservation.abort();
            }
            (Err(ResourceError::LimitExceeded { limit: 5, .. }), Ok(())) => {
                assert_eq!(domain.usage().total(ResourceKind::Threads), 0);
            }
            (reserve, limit) => panic!(
                "non-linearizable reserve/limit outcome: reserve_ok={}, limit={limit:?}",
                reserve.is_ok()
            ),
        }
    }
}

#[test]
fn charge_ownership_retains_the_leaf_and_every_ancestor() {
    let root = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let child = crate::require_ok(root.try_new_child(ResourceLimits::UNLIMITED));
    assert_eq!(child.parent_id(), Some(root.id()));
    let committed =
        crate::require_ok(child.reserve(amount(ResourceKind::UserMappings, 2))).commit();

    drop(child);
    assert!(root.begin_retirement().is_ok());
    let draining = crate::require_ok(root.retirement_snapshot());
    assert!(!draining.is_quiescent());
    assert_eq!(draining.active_children, 1);
    assert_eq!(root.finish_retirement(), Err(ResourceError::ActiveChildren));
    assert_eq!(root.usage().committed(ResourceKind::UserMappings), 2);
    drop(committed);

    assert_eq!(root.usage().total(ResourceKind::UserMappings), 0);
    assert!(crate::require_ok(root.retirement_snapshot()).is_quiescent());
    assert!(root.finish_retirement().is_ok());
    assert!(matches!(
        root.reserve(amount(ResourceKind::UserMappings, 1)),
        Err(ResourceError::DomainInactive(_))
    ));
    assert!(matches!(
        root.try_new_child(ResourceLimits::UNLIMITED),
        Err(ResourceError::DomainInactive(_))
    ));
}

#[test]
fn ancestor_retirement_closes_descendant_admission_but_drains_prior_work() {
    let root = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let child = crate::require_ok(root.try_new_child(ResourceLimits::UNLIMITED));
    let leaf = crate::require_ok(child.try_new_child(ResourceLimits::UNLIMITED));
    let admitted = crate::require_ok(leaf.reserve(amount(ResourceKind::Timers, 1)));

    assert!(root.begin_retirement().is_ok());
    assert!(matches!(
        leaf.reserve(amount(ResourceKind::Timers, 1)),
        Err(ResourceError::DomainInactive(domain)) if domain == root.id()
    ));
    assert!(matches!(
        leaf.try_new_child(ResourceLimits::UNLIMITED),
        Err(ResourceError::DomainInactive(domain)) if domain == root.id()
    ));

    let committed = admitted.commit();
    assert_eq!(root.usage().committed(ResourceKind::Timers), 1);
    drop(committed);
    drop(leaf);
    drop(child);
    assert!(crate::require_ok(root.retirement_snapshot()).is_quiescent());
    assert!(root.finish_retirement().is_ok());
}

#[test]
fn child_allocation_failure_restores_registration_and_metadata_charge() {
    let root = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    root.fail_next_child_allocation_for_test();

    assert!(matches!(
        root.try_new_child(ResourceLimits::UNLIMITED),
        Err(ResourceError::Allocation)
    ));
    assert_eq!(root.usage().total(ResourceKind::KernelObjects), 0);
    assert_eq!(root.usage().total(ResourceKind::KernelMemoryBytes), 0);
    assert!(root.begin_retirement().is_ok());
    assert!(crate::require_ok(root.retirement_snapshot()).is_quiescent());
    assert!(root.finish_retirement().is_ok());
}

#[test]
fn child_metadata_is_parent_sponsored_for_the_complete_child_lifetime() {
    let probe = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let child = crate::require_ok(probe.try_new_child(ResourceLimits::UNLIMITED));
    let metadata_bytes = probe.usage().committed(ResourceKind::KernelMemoryBytes);
    assert!(metadata_bytes > 0);
    assert_eq!(probe.usage().committed(ResourceKind::KernelObjects), 1);
    drop(child);
    assert_eq!(probe.usage().total(ResourceKind::KernelMemoryBytes), 0);
    assert_eq!(probe.usage().total(ResourceKind::KernelObjects), 0);

    let denied = crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::KernelObjects, 0),
    ));
    assert!(matches!(
        denied.try_new_child(ResourceLimits::UNLIMITED),
        Err(ResourceError::LimitExceeded {
            resource: ResourceKind::KernelObjects,
            ..
        })
    ));
}

#[test]
fn concurrent_release_snapshots_never_invert_pending_and_total() {
    let domain = Arc::new(crate::require_ok(ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED,
    )));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let worker_domain = domain.clone();
        workers.push(std::thread::spawn(move || {
            for _ in 0..2_000 {
                let request = ResourceAmount::ZERO
                    .with(ResourceKind::IpcMessages, 1)
                    .with(ResourceKind::IpcBytes, 64);
                drop(crate::require_ok(worker_domain.reserve(request)).commit());
            }
        }));
    }

    while workers.iter().any(|worker| !worker.is_finished()) {
        let usage = domain.usage();
        for kind in [ResourceKind::IpcMessages, ResourceKind::IpcBytes] {
            assert!(usage.pending(kind) <= usage.total(kind));
            let _ = usage.committed(kind);
        }
        std::thread::yield_now();
    }
    for worker in workers {
        assert!(worker.join().is_ok());
    }
    assert_eq!(domain.usage().total(ResourceKind::IpcMessages), 0);
    assert_eq!(domain.usage().total(ResourceKind::IpcBytes), 0);
}

#[test]
fn hierarchy_depth_is_bounded_before_child_registration() {
    let root = crate::require_ok(ResourceDomain::try_new_root(ResourceLimits::UNLIMITED));
    let mut leaf = root.clone();
    for _ in 1..32 {
        leaf = crate::require_ok(leaf.try_new_child(ResourceLimits::UNLIMITED));
    }
    assert!(matches!(
        leaf.try_new_child(ResourceLimits::UNLIMITED),
        Err(ResourceError::HierarchyTooDeep)
    ));
    drop(leaf);
    drop(root);
}
