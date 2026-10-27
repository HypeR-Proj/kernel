// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub(crate) use crate::capability_core::kernel::authority;
use crate::scheduler_policy_model as policy;
#[path = "../../../../src/task/scheduling_authority.rs"]
mod grants;

use authority::Rights;
use grants::{SCHEDULING_RIGHTS, SchedulingAuthority, SchedulingLimits};
use policy::{
    BudgetParameters, FairBackend, SchedulingParameters, SchedulingRole, ThreadPriority,
    ThreadWeight,
};

fn fair(weight: u16, band: u8, role: SchedulingRole) -> SchedulingParameters {
    crate::require_some(SchedulingParameters::fair(
        crate::require_some(ThreadWeight::new(weight)),
        5_000_000,
        ThreadPriority::new(band),
        role,
        FairBackend::Stride,
    ))
}

#[test]
fn ordinary_write_cannot_change_scheduling_and_demotion_cannot_be_undone() {
    let old = fair(16, 128, SchedulingRole::Protected);
    let lower = fair(8, 160, SchedulingRole::Protected);
    let limits = SchedulingLimits::inherited(old);
    assert!(
        SchedulingAuthority::new(Rights::WRITE)
            .validate_change(old, lower, limits)
            .is_err()
    );
    let own = SchedulingAuthority::new(Rights::SCHED_LOWER);
    assert!(own.validate_change(old, lower, limits).is_ok());
    assert!(own.validate_change(lower, old, limits).is_err());
    assert!(own.validate_change(lower, lower, limits).is_ok());
}

#[test]
fn priority_class_and_budget_are_independent_authorities() {
    let old = fair(16, 128, SchedulingRole::Protected);
    let raised = fair(32, 64, SchedulingRole::Protected);
    let limits = SchedulingLimits::SYSTEM;
    assert!(
        SchedulingAuthority::new(Rights::SCHED_CLASS)
            .validate_change(old, raised, limits)
            .is_err()
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_PRIORITY)
            .validate_change(old, raised, limits)
            .is_ok()
    );
    let service = fair(16, 128, SchedulingRole::Priority);
    assert!(
        SchedulingAuthority::new(Rights::SCHED_PRIORITY)
            .validate_change(old, service, limits)
            .is_err()
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_CLASS)
            .validate_change(old, service, limits)
            .is_ok()
    );
    let budget = SchedulingParameters::budget(
        crate::require_some(BudgetParameters::new(7, 10, 5)),
        ThreadPriority::NORMAL,
        SchedulingRole::Protected,
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_CLASS)
            .validate_change(old, budget, limits)
            .is_err()
    );
    let full = SchedulingAuthority::new(SCHEDULING_RIGHTS);
    assert!(full.validate_change(old, budget, limits).is_ok());
    let reduced = SchedulingParameters::budget(
        crate::require_some(BudgetParameters::new(6, 10, 5)),
        ThreadPriority::NORMAL,
        SchedulingRole::Protected,
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_LOWER)
            .validate_change(budget, reduced, limits)
            .is_err()
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_BUDGET)
            .validate_change(budget, reduced, limits)
            .is_ok()
    );
}

#[test]
fn object_ceiling_is_not_enlarged_by_a_more_powerful_handle() {
    let old = fair(16, 128, SchedulingRole::Ordinary);
    let limits = SchedulingLimits::inherited(old);
    let full = SchedulingAuthority::new(SCHEDULING_RIGHTS);
    assert!(
        full.validate_change(old, fair(17, 128, SchedulingRole::Ordinary), limits)
            .is_err()
    );
    assert!(
        full.validate_change(old, fair(16, 127, SchedulingRole::Ordinary), limits)
            .is_err()
    );
    assert!(
        full.validate_change(old, fair(16, 128, SchedulingRole::Priority), limits)
            .is_err()
    );
    assert!(
        full.validate_change(old, SchedulingParameters::idle(), limits)
            .is_err()
    );
}

#[test]
fn quantum_and_fifo_priority_cannot_bypass_priority_permission() {
    let old = fair(16, 128, SchedulingRole::Protected);
    let changed = crate::require_some(SchedulingParameters::fair(
        ThreadWeight::DEFAULT,
        10_000_000,
        ThreadPriority::NORMAL,
        SchedulingRole::Protected,
        FairBackend::Stride,
    ));
    assert!(
        SchedulingAuthority::new(Rights::SCHED_LOWER)
            .validate_change(old, changed, SchedulingLimits::SYSTEM)
            .is_err()
    );
    let fifo = SchedulingParameters::fifo(
        ThreadPriority::new(100),
        ThreadPriority::NORMAL,
        SchedulingRole::Priority,
    );
    let high = SchedulingParameters::fifo(
        ThreadPriority::new(99),
        ThreadPriority::NORMAL,
        SchedulingRole::Priority,
    );
    assert!(
        SchedulingAuthority::new(Rights::SCHED_LOWER)
            .validate_change(fifo, high, SchedulingLimits::SYSTEM)
            .is_err()
    );
}

#[test]
fn readonly_builder_cannot_change_child_grants_through_parameter_noop() {
    let elevated = Rights::SCHED_LOWER.union(Rights::SCHED_PRIORITY);
    let readonly = SchedulingAuthority::new(Rights::INSPECT);
    assert!(
        readonly
            .validate_delegation(elevated, Rights::NONE)
            .is_err()
    );
    assert!(
        readonly
            .validate_delegation(Rights::SCHED_LOWER, Rights::NONE)
            .is_ok()
    );
    let lower = SchedulingAuthority::new(Rights::SCHED_LOWER);
    assert!(lower.validate_delegation(elevated, Rights::NONE).is_ok());
    assert!(
        lower
            .validate_delegation(Rights::SCHED_LOWER, Rights::SCHED_PRIORITY)
            .is_err()
    );
    let priority = SchedulingAuthority::new(Rights::SCHED_PRIORITY);
    assert!(
        priority
            .validate_delegation(Rights::SCHED_LOWER, Rights::SCHED_PRIORITY)
            .is_ok()
    );
}
