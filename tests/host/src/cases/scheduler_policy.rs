// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host checks for static scheduler classes and placement invariants.

#[path = "../../../../src/kernel/task/scheduler/balance_policy.rs"]
mod balance_policy;
use crate::scheduler_policy_model as policy;

use hyper::cpu::CpuIndex;
use policy::{
    CpuMask, PlacementPolicy, SchedulingClass, SchedulingPolicy, ThreadPlacement, ThreadPriority,
};

#[test]
fn scheduling_profiles_encode_only_class_relevant_parameters() {
    assert_eq!(policy::PRIORITY_LEVELS, 256);
    let fifo = SchedulingPolicy::fifo(ThreadPriority::NORMAL);
    assert_eq!(fifo.class(), SchedulingClass::RealTime);
    assert_eq!(fifo.priority(), Some(ThreadPriority::NORMAL));
    assert_eq!(SchedulingPolicy::fair().class(), SchedulingClass::Fair);
    assert_eq!(SchedulingPolicy::fair().priority(), None);
    assert_eq!(SchedulingPolicy::Budget.class(), SchedulingClass::Budget);
    assert_eq!(SchedulingPolicy::Budget.priority(), None);
    assert_eq!(SchedulingPolicy::Idle.class(), SchedulingClass::Idle);
    assert_eq!(SchedulingPolicy::Idle.priority(), None);
    assert!(ThreadPriority::HIGHEST < ThreadPriority::NORMAL);
    assert!(ThreadPriority::NORMAL < ThreadPriority::LOWEST);
    assert_eq!(ThreadPriority::new(7).get(), 7);
}

#[test]
fn placement_requires_assignment_to_satisfy_affinity() {
    let cpu0 = crate::require_some(CpuIndex::new(0));
    let cpu1 = crate::require_some(CpuIndex::new(1));
    let affinity = CpuMask::EMPTY.with_cpu(cpu0).with_cpu(cpu1);
    let placement = crate::require_some(ThreadPlacement::new(
        cpu1,
        affinity,
        PlacementPolicy::Movable,
    ));
    assert_eq!(placement.assigned_cpu(), cpu1);
    assert!(placement.affinity().contains(cpu0));
    assert!(placement.affinity().contains(cpu1));
    assert_eq!(
        ThreadPlacement::new(cpu0, CpuMask::single(cpu1), PlacementPolicy::Movable),
        None
    );
    assert_eq!(
        ThreadPlacement::new(cpu0, affinity, PlacementPolicy::Pinned(cpu1)),
        None
    );

    let pinned = ThreadPlacement::pinned(cpu0);
    assert_eq!(pinned.assigned_cpu(), cpu0);
    assert!(pinned.affinity().contains(cpu0));
    assert!(!pinned.affinity().contains(cpu1));
    assert_eq!(pinned.policy(), PlacementPolicy::Pinned(cpu0));
    assert_eq!(pinned.last_cpu(), Some(cpu0));

    let movable = ThreadPlacement::movable(cpu1);
    assert_eq!(movable.policy(), PlacementPolicy::Movable);
    assert!(movable.affinity().contains(cpu0));
    assert!(movable.affinity().contains(cpu1));
    let running = crate::require_some(movable.mark_running(cpu1));
    assert_eq!(running.last_cpu(), Some(cpu1));
    assert_eq!(movable.mark_running(cpu0), None);

    let constrained_affinity = CpuMask::EMPTY.with_cpu(cpu0).with_cpu(cpu1);
    let constrained = crate::require_some(ThreadPlacement::movable_with_affinity(
        cpu1,
        constrained_affinity,
    ));
    assert_eq!(constrained.assigned_cpu(), cpu1);
    assert_eq!(constrained.affinity(), constrained_affinity);
    assert_eq!(constrained.policy(), PlacementPolicy::Movable);
    let narrowed = crate::require_some(constrained.with_affinity(CpuMask::single(cpu1)));
    assert_eq!(narrowed.assigned_cpu(), cpu1);
    assert_eq!(narrowed.affinity(), CpuMask::single(cpu1));
    assert_eq!(constrained.with_affinity(CpuMask::single(cpu0)), None);
    let migrated =
        crate::require_some(constrained.reassign_with_affinity(cpu0, CpuMask::single(cpu0)));
    assert_eq!(migrated.assigned_cpu(), cpu0);
    assert_eq!(migrated.affinity(), CpuMask::single(cpu0));
    assert_eq!(
        constrained.reassign_with_affinity(cpu1, CpuMask::single(cpu0)),
        None
    );
    assert_eq!(
        pinned.reassign_with_affinity(cpu0, CpuMask::single(cpu0)),
        None
    );
    assert_eq!(
        ThreadPlacement::movable_with_affinity(cpu1, CpuMask::single(cpu0)),
        None
    );

    assert!(CpuMask::EMPTY.is_empty());
    assert_eq!(
        constrained_affinity.without_cpu(cpu0),
        CpuMask::single(cpu1)
    );
}

#[test]
fn initial_balance_prefers_less_work_but_keeps_locality_on_ties() {
    use balance_policy::least_loaded;

    assert_eq!(
        least_loaded(0, [(0, 4), (1, 0), (2, 2)].into_iter()),
        Some(1)
    );
    assert_eq!(
        least_loaded(2, [(0, 1), (1, 1), (2, 1)].into_iter()),
        Some(2)
    );
    assert_eq!(least_loaded(9, [(3, 1), (5, 1)].into_iter()), Some(3));
    assert_eq!(least_loaded(0, [(4, 7)].into_iter()), Some(4));
    assert_eq!(least_loaded(0, core::iter::empty()), None);
}

#[test]
fn balance_never_swaps_a_single_runnable_thread_between_equal_cpus() {
    for source in 0..16 {
        for target in 0..16 {
            let before = source * source + target * target;
            let improves = balance_policy::improves_balance(source, target);
            if improves {
                let after = (source - 1) * (source - 1) + (target + 1) * (target + 1);
                assert!(after < before);
            } else if source > 0 {
                let after = (source - 1) * (source - 1) + (target + 1) * (target + 1);
                assert!(after >= before);
            }
        }
    }
    assert!(!balance_policy::improves_balance(0, u32::MAX));
    assert!(!balance_policy::improves_balance(u32::MAX, u32::MAX - 1));
    assert!(balance_policy::improves_balance(u32::MAX, 0));
}
