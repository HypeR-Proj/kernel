// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Execute the production policy accounting and arbitration without machine state.

#[path = "../../../../src/task/scheduler/algorithms/mod.rs"]
#[allow(
    dead_code,
    reason = "The host harness imports production algorithms without CPU queue integration"
)]
mod algorithms;
use crate::scheduler_policy_model as policy;

use algorithms::arbitration::{
    ArbitrationConfig, ArbitrationOrder, Candidate, CpuWindow, Demand, DispatchMode,
};
use algorithms::{BudgetRuntime, FairRuntime, PolicyRuntime};
use policy::{
    BudgetParameters, FairBackend, SchedulingParameters, SchedulingRole, ThreadPriority,
    ThreadWeight,
};

fn fair(role: SchedulingRole) -> SchedulingParameters {
    crate::require_some(SchedulingParameters::fair(
        ThreadWeight::DEFAULT,
        10,
        ThreadPriority::NORMAL,
        role,
        FairBackend::Stride,
    ))
}

fn budget(q: u64, p: u64, burst: u64) -> BudgetParameters {
    crate::require_some(BudgetParameters::new(q, p, burst))
}

fn window() -> CpuWindow {
    CpuWindow::new(crate::require_some(ArbitrationConfig::new(
        100,
        20,
        10,
        ArbitrationOrder::DEFAULT,
    )))
}

fn candidate(parameters: SchedulingParameters, budget_available: bool) -> Candidate {
    Candidate {
        parameters,
        budget_available,
        fair_pass: 0,
        turn: 0,
    }
}

#[test]
fn shared_admission_tiers_do_not_compare_different_algorithm_key_units() {
    let mut window = window();
    let mut native = candidate(fair(SchedulingRole::Priority), true);
    // A long-running stride domain must not eventually lose its admission
    // order merely because pass overtakes the unrelated Budget cursor value.
    native.fair_pass = u128::MAX;
    let mut vm = candidate(
        SchedulingParameters::budget(
            budget(80, 100, 50),
            ThreadPriority::NORMAL,
            SchedulingRole::Priority,
        ),
        true,
    );
    vm.turn = 1;
    let fifo = candidate(
        SchedulingParameters::fifo(
            ThreadPriority::NORMAL,
            ThreadPriority::NORMAL,
            SchedulingRole::Priority,
        ),
        true,
    );
    let demand = Demand {
        fair: true,
        eligible_budget: true,
        any_budget: true,
        ..Demand::default()
    };
    let (native_key, _) = crate::require_some(window.candidate_key(0, native, demand));
    let (vm_key, _) = crate::require_some(window.candidate_key(0, vm, demand));
    let (fifo_key, _) = crate::require_some(window.candidate_key(0, fifo, demand));
    assert!(fifo_key < native_key);
    assert!(native_key < vm_key);
    // Priority allowance still ends the common tier and lets Budget run.
    assert!(window.account_dispatch(0, 20, native.parameters, DispatchMode::Priority));
    let (native_key, _) = crate::require_some(window.candidate_key(20, native, demand));
    let (vm_key, _) = crate::require_some(window.candidate_key(20, vm, demand));
    assert!(vm_key < native_key);
}

#[test]
fn algorithm_parameters_reject_zero_and_inconsistent_budgets() {
    assert!(ThreadWeight::new(0).is_none());
    assert!(ThreadWeight::new(ThreadWeight::MAX + 1).is_none());
    assert!(BudgetParameters::new(0, 10, 1).is_none());
    assert!(BudgetParameters::new(11, 10, 1).is_none());
    assert!(BudgetParameters::new(1, 10, 0).is_none());
    assert!(ArbitrationConfig::new(100, 90, 10, ArbitrationOrder::DEFAULT).is_none());
}

#[test]
fn image_admission_rejects_mixed_fair_backends_and_budget_periods() {
    let config = crate::require_some(ArbitrationConfig::new(
        100,
        20,
        10,
        ArbitrationOrder::DEFAULT,
    ));
    let stride = fair(SchedulingRole::Protected);
    let rr = crate::require_some(SchedulingParameters::fair(
        stride.weight(),
        stride.quantum_ns(),
        stride.band(),
        stride.role(),
        FairBackend::RoundRobin,
    ));
    assert!(config.admits(stride, FairBackend::Stride));
    assert!(!config.admits(rr, FairBackend::Stride));
    assert!(config.admits(rr, FairBackend::RoundRobin));
    assert!(!config.admits(stride, FairBackend::RoundRobin));
    for backend in [FairBackend::Stride, FairBackend::RoundRobin] {
        assert!(!config.admits(SchedulingParameters::idle(), backend));
        assert!(config.admits(
            SchedulingParameters::budget(
                budget(80, 100, 50),
                ThreadPriority::NORMAL,
                SchedulingRole::Ordinary,
            ),
            backend,
        ));
        assert!(!config.admits(
            SchedulingParameters::budget(
                budget(80, 200, 50),
                ThreadPriority::NORMAL,
                SchedulingRole::Ordinary,
            ),
            backend,
        ));
        assert!(config.admits(
            SchedulingParameters::fifo(
                ThreadPriority::NORMAL,
                ThreadPriority::NORMAL,
                SchedulingRole::Priority,
            ),
            backend,
        ));
    }
}

#[test]
fn stride_accounts_short_execution_and_weighted_share() {
    let low = crate::require_some(ThreadWeight::new(1));
    let high = crate::require_some(ThreadWeight::new(4));
    let mut runtimes = [FairRuntime::new(10), FairRuntime::new(10)];
    let mut executions = [0, 0];
    for _ in 0..1000 {
        let chosen = usize::from(runtimes[1].pass() < runtimes[0].pass());
        runtimes[chosen].account(1, if chosen == 0 { low } else { high });
        executions[chosen] += 1;
    }
    assert_eq!(executions, [200, 800]);
    assert_eq!(runtimes[0].pass(), runtimes[1].pass());
}

#[test]
fn fair_migration_preserves_execution_debt_and_remaining_turn() {
    let mut runtime = FairRuntime::new(100);
    runtime.account(25, ThreadWeight::DEFAULT);
    let debt = runtime.pass();
    runtime.migrate_floor(0, 50_000);
    assert_eq!(runtime.pass(), 50_000 + debt);
    assert_eq!(runtime.remaining_ns(), 75);
    runtime.admit(60_000);
    assert_eq!(runtime.pass(), 60_000);
    runtime.expire_turn();
    assert_eq!(runtime.remaining_ns(), 0);
}

#[test]
fn fair_quantum_edits_preserve_consumed_time_and_stride_debt() {
    let mut runtime = FairRuntime::new(100);
    runtime.account(30, ThreadWeight::DEFAULT);
    let pass = runtime.pass();
    runtime.reconfigure(100, 50);
    assert_eq!(runtime.remaining_ns(), 20);
    runtime.reconfigure(50, 100);
    assert_eq!(runtime.remaining_ns(), 20);
    assert_eq!(runtime.pass(), pass);
    runtime.reconfigure(100, 10);
    runtime.reconfigure(10, 100);
    assert_eq!(runtime.remaining_ns(), 0);
    runtime.finish_turn(100);
    assert_eq!(runtime.remaining_ns(), 100);
}

#[test]
fn budget_edits_cannot_refresh_a_window_or_an_uninterrupted_burst() {
    let original = budget(80, 100, 60);
    let reduced = budget(40, 100, 40);
    let mut runtime = BudgetRuntime::new(original);
    assert!(runtime.account_interval(0, 30, original));
    assert!(runtime.reconfigure(original, reduced));
    assert_eq!(runtime.remaining_ns(), 10);
    assert_eq!(runtime.burst_remaining_ns(), 10);
    assert!(runtime.reconfigure(reduced, original));
    assert_eq!(runtime.remaining_ns(), 10);
    assert_eq!(runtime.burst_remaining_ns(), 10);
    let minimal = budget(1, 100, 1);
    assert!(runtime.reconfigure(original, minimal));
    assert!(runtime.reconfigure(minimal, original));
    assert_eq!(runtime.remaining_ns(), 0);
    assert_eq!(runtime.burst_remaining_ns(), 0);
    runtime.refresh(100, original);
    assert_eq!(runtime.remaining_ns(), 80);
    assert_eq!(runtime.burst_remaining_ns(), 0);
    runtime.finish_turn(original);
    assert_eq!(runtime.burst_remaining_ns(), 60);
    assert!(!runtime.reconfigure(original, budget(80, 200, 60)));
}

#[test]
fn fair_backend_can_change_selection_without_changing_execution_accounting() {
    let stride = fair(SchedulingRole::Protected);
    let rr = crate::require_some(SchedulingParameters::fair(
        stride.weight(),
        stride.quantum_ns(),
        stride.band(),
        stride.role(),
        FairBackend::RoundRobin,
    ));
    let mut runtime = PolicyRuntime::new(stride);
    assert!(runtime.account(stride, 0, 5));
    assert_ne!(runtime.candidate_key(stride, 7).0, 0);
    assert_eq!(runtime.candidate_key(rr, 7), (0, 7));
}

#[test]
fn sole_budget_borrower_has_no_scheduler_deadline() {
    let params = SchedulingParameters::budget(
        budget(30, 100, 10),
        ThreadPriority::NORMAL,
        SchedulingRole::Ordinary,
    );
    let mut runtime = PolicyRuntime::new(params);
    assert!(runtime.account(params, 0, 250));
    assert_eq!(runtime.next_deadline(250, params, false, true), None);
    assert_eq!(runtime.next_deadline(250, params, true, true), Some(250));
    runtime.finish_turn(params);
    assert_eq!(runtime.next_deadline(250, params, true, true), Some(260));
}

#[test]
fn budget_period_rollover_does_not_refill_continuous_dispatch() {
    let params = budget(100, 100, 30);
    let mut runtime = BudgetRuntime::new(params);
    assert!(runtime.account_interval(90, 110, params));
    assert_eq!(runtime.remaining_ns(), 90);
    assert_eq!(runtime.burst_remaining_ns(), 10);
    runtime.refresh(200, params);
    assert_eq!(runtime.remaining_ns(), 100);
    assert_eq!(runtime.burst_remaining_ns(), 10);
}

#[test]
fn long_budget_borrowing_only_debits_the_final_period() {
    let params = budget(30, 100, 10);
    let mut runtime = BudgetRuntime::new(params);
    assert!(runtime.account_interval(0, 1005, params));
    assert_eq!(runtime.remaining_ns(), 25);
    assert_eq!(runtime.burst_remaining_ns(), 0);
    runtime.refresh(1006, params);
    assert_eq!(runtime.remaining_ns(), 25);
    let before = runtime;
    assert!(!runtime.account_interval(1006, 1005, params));
    assert_eq!(runtime, before);
}

#[test]
fn interrupt_boundaries_do_not_reset_budget_or_fair_accounting() {
    let params = budget(30, 100, 20);
    let mut single = BudgetRuntime::new(params);
    let mut split = single;
    assert!(single.account_interval(85, 115, params));
    for (start, end) in [(85, 90), (90, 100), (100, 105), (105, 115)] {
        assert!(split.account_interval(start, end, params));
    }
    assert_eq!(single, split);
    let mut one = FairRuntime::new(30);
    let mut many = one;
    one.account(30, crate::require_some(ThreadWeight::new(7)));
    for _ in 0..30 {
        many.account(1, crate::require_some(ThreadWeight::new(7)));
    }
    assert_eq!(one, many);
}

#[test]
fn priority_allowance_exhaustion_yields_to_budget_work() {
    let mut cpu = window();
    let service = fair(SchedulingRole::Priority);
    let vm = SchedulingParameters::budget(
        budget(60, 100, 10),
        ThreadPriority::NORMAL,
        SchedulingRole::Ordinary,
    );
    let demand = Demand {
        fair: true,
        any_budget: true,
        eligible_budget: true,
        protected: false,
        window_sensitive: true,
    };
    let before = crate::require_some(cpu.candidate_key(0, candidate(service, false), demand));
    let guest = crate::require_some(cpu.candidate_key(0, candidate(vm, true), demand));
    assert!(before.0 < guest.0);
    assert!(cpu.account_interval(0, 20, service));
    let after = crate::require_some(cpu.candidate_key(20, candidate(service, false), demand));
    assert!(guest.0 < after.0);
    assert_eq!(after.1, DispatchMode::Ordinary);
}

#[test]
fn protected_floor_cannot_be_satisfied_by_priority_service() {
    let mut cpu = window();
    let service = fair(SchedulingRole::Priority);
    assert!(cpu.account_interval(0, 90, service));
    assert_eq!(cpu.protected_remaining_ns(), 10);
    let demand = Demand {
        protected: true,
        fair: true,
        ..Demand::default()
    };
    let low = crate::require_some(cpu.candidate_key(
        90,
        candidate(fair(SchedulingRole::Protected), false),
        demand,
    ));
    let high = crate::require_some(cpu.candidate_key(90, candidate(service, false), demand));
    assert!(low.0 < high.0);
    assert_eq!(low.1, DispatchMode::Protected);
}

#[test]
fn cpu_ledger_is_not_replenished_by_new_threads_or_solo_observation() {
    let mut cpu = window();
    let service = fair(SchedulingRole::Priority);
    assert!(cpu.account_interval(0, 20, service));
    for now in 20..100 {
        assert_eq!(
            cpu.next_deadline(now, DispatchMode::Ordinary, false, Demand::default()),
            None
        );
        assert_eq!(cpu.priority_remaining_ns(), 0);
    }
    cpu.refresh(100);
    assert_eq!(cpu.priority_remaining_ns(), 20);
}

#[test]
fn priority_fifo_is_throttled_but_explicit_strict_fifo_remains_urgent() {
    let mut cpu = window();
    let service = SchedulingParameters::fifo(
        ThreadPriority::HIGHEST,
        ThreadPriority::HIGHEST,
        SchedulingRole::Priority,
    );
    let urgent = SchedulingParameters::fifo(
        ThreadPriority::HIGHEST,
        ThreadPriority::HIGHEST,
        SchedulingRole::Ordinary,
    );
    let low = fair(SchedulingRole::Protected);
    assert!(cpu.account_interval(0, 90, service));
    let demand = Demand {
        protected: true,
        fair: true,
        ..Demand::default()
    };
    let low_key = crate::require_some(cpu.candidate_key(90, candidate(low, false), demand));
    let service_key = crate::require_some(cpu.candidate_key(90, candidate(service, false), demand));
    let urgent_key = crate::require_some(cpu.candidate_key(90, candidate(urgent, false), demand));
    assert!(urgent_key.0 < low_key.0 && low_key.0 < service_key.0);
}

#[test]
fn budget_borrowing_never_displaces_runnable_fair_work() {
    let mut cpu = window();
    let vm = SchedulingParameters::budget(
        budget(60, 100, 10),
        ThreadPriority::NORMAL,
        SchedulingRole::Ordinary,
    );
    assert!(
        cpu.candidate_key(
            70,
            candidate(vm, false),
            Demand {
                fair: true,
                ..Demand::default()
            }
        )
        .is_none()
    );
    let borrowed =
        crate::require_some(cpu.candidate_key(70, candidate(vm, false), Demand::default()));
    assert_eq!(borrowed.1, DispatchMode::Borrow);
}

#[test]
fn preferred_fair_spends_shared_allowance_and_still_yields_to_budget() {
    let config = crate::require_some(ArbitrationConfig::new(
        100,
        20,
        10,
        ArbitrationOrder::DEFAULT,
    ))
    .with_prefer_fair(true);
    let mut cpu = CpuWindow::new(config);
    let app = fair(SchedulingRole::Protected);
    let vm = SchedulingParameters::budget(
        budget(70, 100, 30),
        ThreadPriority::NORMAL,
        SchedulingRole::Ordinary,
    );
    let demand = Demand {
        fair: true,
        any_budget: true,
        eligible_budget: true,
        protected: true,
        window_sensitive: true,
    };
    let first = crate::require_some(cpu.candidate_key(0, candidate(app, false), demand));
    assert_eq!(first.1, DispatchMode::Priority);
    assert!(cpu.account_dispatch(0, 20, app, first.1));
    assert_eq!(cpu.priority_remaining_ns(), 0);
    assert_eq!(cpu.protected_remaining_ns(), 0);
    let native = crate::require_some(cpu.candidate_key(20, candidate(app, false), demand));
    let guest = crate::require_some(cpu.candidate_key(20, candidate(vm, true), demand));
    assert!(guest.0 < native.0);
}

#[test]
fn priority_replenishment_stays_event_driven_without_a_budget_thread() {
    let mut cpu = window();
    let service = fair(SchedulingRole::Priority);
    assert!(cpu.account_interval(0, 20, service));
    let demand = Demand {
        fair: true,
        window_sensitive: true,
        ..Demand::default()
    };
    assert_eq!(
        cpu.next_deadline(20, DispatchMode::Ordinary, true, demand),
        Some(100)
    );
    assert_eq!(
        cpu.next_deadline(20, DispatchMode::Ordinary, false, demand),
        None
    );
}

mod ready_index_fixture {
    use super::algorithms::ready_index::{Links, Nodes};
    use std::collections::BTreeSet;
    pub(super) struct Registry {
        pub(super) nodes: Vec<(u64, Links<usize>)>,
    }
    impl Nodes for Registry {
        type Id = usize;
        type Key = (u64, usize);
        type Error = ();
        fn key(&mut self, id: usize) -> Result<Self::Key, ()> {
            self.nodes.get(id).map(|entry| (entry.0, id)).ok_or(())
        }
        fn links(&mut self, id: usize) -> Result<Links<usize>, ()> {
            self.nodes.get(id).map(|entry| entry.1).ok_or(())
        }
        fn set_links(&mut self, id: usize, links: Links<usize>) -> Result<(), ()> {
            self.nodes
                .get_mut(id)
                .map(|entry| entry.1 = links)
                .ok_or(())
        }
        fn corrupt(&self) {}
    }
    pub(super) fn verify(registry: &Registry, active: &BTreeSet<(u64, usize)>) {
        if active.is_empty() {
            return;
        }
        let mut roots: BTreeSet<usize> = active.iter().map(|entry| entry.1).collect();
        for (_, id) in active {
            let links = registry.nodes[*id].1;
            if let Some(left) = links.left {
                assert!(active.contains(&(registry.nodes[left].0, left)));
                roots.remove(&left);
            }
            if let Some(right) = links.right {
                assert!(active.contains(&(registry.nodes[right].0, right)));
                roots.remove(&right);
            }
        }
        assert_eq!(roots.len(), 1);
        fn visit(
            registry: &Registry,
            id: usize,
            seen: &mut BTreeSet<usize>,
        ) -> (u8, (u64, usize), (u64, usize)) {
            assert!(seen.insert(id));
            let (value, links) = registry.nodes[id];
            let key = (value, id);
            let (mut min, mut max) = (key, key);
            let lh = if let Some(left) = links.left {
                let (h, a, b) = visit(registry, left, seen);
                assert!(b < key);
                min = a;
                h
            } else {
                0
            };
            let rh = if let Some(right) = links.right {
                let (h, a, b) = visit(registry, right, seen);
                assert!(key < a);
                max = b;
                h
            } else {
                0
            };
            assert!((i16::from(lh) - i16::from(rh)).abs() <= 1);
            assert_eq!(links.height, 1 + lh.max(rh));
            (links.height, min, max)
        }
        let mut seen = BTreeSet::new();
        visit(registry, *crate::require_some(roots.first()), &mut seen);
        assert_eq!(seen.len(), active.len());
    }
}

#[test]
fn ready_index_stays_balanced_and_ordered_through_insert_remove_and_rekey() {
    use algorithms::ready_index::{Links, MutationScratch, Tree};
    use ready_index_fixture::{Registry, verify};
    use std::collections::BTreeSet;
    let mut registry = Registry {
        nodes: (0..257)
            .map(|id| ((id % 13) as u64, Links::EMPTY))
            .collect(),
    };
    let mut tree = Tree::new();
    let mut scratch = Box::new(MutationScratch::new(0));
    let mut active = BTreeSet::new();
    for step in 0..257 {
        let id = step * 97 % 257;
        crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
        active.insert((registry.nodes[id].0, id));
        verify(&registry, &active);
    }
    for step in 0..257 {
        let id = step * 73 % 257;
        assert!(crate::require_ok(tree.contains(&mut registry, id)));
        crate::require_ok(tree.remove(&mut registry, id, &mut scratch));
        active.remove(&(registry.nodes[id].0, id));
        verify(&registry, &active);
        assert_eq!(
            crate::require_ok(tree.first(&mut registry)),
            active.first().map(|entry| entry.1)
        );
        if step % 5 == 0 {
            registry.nodes[id].0 = 1000 + step as u64;
            crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
            active.insert((registry.nodes[id].0, id));
            verify(&registry, &active);
        }
    }
    while let Some(id) = crate::require_ok(tree.first(&mut registry)) {
        crate::require_ok(tree.remove(&mut registry, id, &mut scratch));
        active.remove(&(registry.nodes[id].0, id));
        verify(&registry, &active);
    }
    assert!(active.is_empty());
    assert!(tree.is_empty());
}

#[test]
fn ready_index_mixed_random_changes_preserve_links_and_order() {
    use algorithms::ready_index::{Links, MutationScratch, Tree};
    use ready_index_fixture::{Registry, verify};
    use std::collections::BTreeSet;
    let mut registry = Registry {
        nodes: (0..257).map(|id| (id as u64, Links::EMPTY)).collect(),
    };
    let mut active = BTreeSet::new();
    let mut tree = Tree::new();
    let mut scratch = Box::new(MutationScratch::new(0));
    let mut random = 0x8c51_e0ab_1932_f6d7_u64;
    for _ in 0..4096 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let id = random as usize % registry.nodes.len();
        if active.remove(&(registry.nodes[id].0, id)) {
            crate::require_ok(tree.remove(&mut registry, id, &mut scratch));
        } else {
            registry.nodes[id].0 = random % 128;
            crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
            active.insert((registry.nodes[id].0, id));
        }
        verify(&registry, &active);
        assert_eq!(
            crate::require_ok(tree.first(&mut registry)),
            active.first().map(|entry| entry.1)
        );
    }
}

#[test]
fn ready_index_two_child_removal_keeps_the_successors_right_subtree() {
    use algorithms::ready_index::{Links, MutationScratch, Tree};
    use ready_index_fixture::{Registry, verify};
    use std::collections::BTreeSet;
    let mut registry = Registry {
        nodes: (0..12).map(|id| (id as u64, Links::EMPTY)).collect(),
    };
    let mut tree = Tree::new();
    let mut scratch = Box::new(MutationScratch::new(0));
    let mut active = BTreeSet::new();
    for id in [4, 2, 8, 1, 3, 5, 10, 6, 9, 11] {
        crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
        active.insert((id as u64, id));
    }
    assert_eq!(registry.nodes[4].1.left, Some(2));
    assert_eq!(registry.nodes[4].1.right, Some(8));
    assert_eq!(registry.nodes[8].1.left, Some(5));
    assert_eq!(registry.nodes[5].1.right, Some(6));
    crate::require_ok(tree.remove(&mut registry, 4, &mut scratch));
    active.remove(&(4, 4));
    verify(&registry, &active);
    assert!(crate::require_ok(tree.contains(&mut registry, 6)));
    // Also cover a successor that is the immediate right child, with an
    // original ancestor still waiting below the path boundary.
    crate::require_ok(tree.remove(&mut registry, 2, &mut scratch));
    active.remove(&(2, 2));
    verify(&registry, &active);
}

#[test]
fn ready_index_handles_large_populations_without_recursive_mutation() {
    use algorithms::ready_index::{Links, MutationScratch, Tree};
    const MAX_NODES: usize = 32_768;
    use ready_index_fixture::{Registry, verify};
    use std::collections::BTreeSet;
    for reverse in [false, true] {
        let mut registry = Registry {
            nodes: (0..MAX_NODES).map(|id| (id as u64, Links::EMPTY)).collect(),
        };
        let mut active = BTreeSet::new();
        let mut tree = Tree::new();
        let mut scratch = Box::new(MutationScratch::new(0));
        for step in 0..MAX_NODES {
            let id = if reverse { MAX_NODES - step - 1 } else { step };
            crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
            active.insert((id as u64, id));
        }
        verify(&registry, &active);
        assert!(registry.nodes.iter().all(|(_, links)| links.height <= 19));
        for step in 0..MAX_NODES {
            // Odd stride visits every slot of the power-of-two registry.
            let id = step * 8191 % MAX_NODES;
            crate::require_ok(tree.remove(&mut registry, id, &mut scratch));
            active.remove(&(id as u64, id));
            if step.is_multiple_of(257) {
                verify(&registry, &active);
            }
            assert_eq!(
                crate::require_ok(tree.first(&mut registry)),
                active.first().map(|entry| entry.1)
            );
        }
        assert!(tree.is_empty());
        assert!(active.is_empty());
    }
}

#[test]
fn ready_index_rejects_duplicate_and_overlong_paths_before_mutation() {
    use algorithms::ready_index::{Links, MutationScratch, Tree};
    use ready_index_fixture::{Registry, verify};
    use std::collections::BTreeSet;
    let mut registry = Registry {
        nodes: (0..65).map(|id| (id as u64, Links::EMPTY)).collect(),
    };
    let mut tree = Tree::new();
    let mut scratch = Box::new(MutationScratch::new(0));
    for id in 0..3 {
        crate::require_ok(tree.insert(&mut registry, id, &mut scratch));
    }
    assert!(tree.insert(&mut registry, 1, &mut scratch).is_err());
    verify(
        &registry,
        &(0..3).map(|id| (id as u64, id)).collect::<BTreeSet<_>>(),
    );
    // Existing root is 1. A forged sorted chain exceeds the bounded path;
    // rejection must occur before any of its links are rewritten.
    for id in 1..64 {
        registry.nodes[id].1 = Links {
            left: None,
            right: Some(id + 1),
            height: 1,
        };
    }
    let links = |registry: &Registry| {
        registry
            .nodes
            .iter()
            .map(|(_, l)| (l.left, l.right, l.height))
            .collect::<Vec<_>>()
    };
    let before = links(&registry);
    assert!(tree.contains(&mut registry, 64).is_err());
    assert!(tree.insert(&mut registry, 64, &mut scratch).is_err());
    assert!(tree.remove(&mut registry, 64, &mut scratch).is_err());
    assert_eq!(links(&registry), before);
    registry.nodes[1].1.left = Some(1);
    assert!(tree.first(&mut registry).is_err());

    // A failed traversal leaves a nonempty scratch path. Another index must
    // reset it before reuse, without touching nodes from the previous tree.
    let mut clean = Registry {
        nodes: (0..9).map(|id| (id as u64, Links::EMPTY)).collect(),
    };
    let mut other = Tree::new();
    for id in [4, 2, 6, 1, 3, 5, 7] {
        crate::require_ok(other.insert(&mut clean, id, &mut scratch));
    }
    crate::require_ok(other.remove(&mut clean, 4, &mut scratch));
    verify(
        &clean,
        &[1, 2, 3, 5, 6, 7]
            .into_iter()
            .map(|id| (id as u64, id))
            .collect(),
    );
}

#[test]
fn stale_epoch_observation_cannot_refill_a_budget() {
    let params = budget(30, 100, 10);
    let mut runtime = BudgetRuntime::new(params);
    assert!(runtime.account_interval(100, 125, params));
    assert_eq!(runtime.remaining_ns(), 5);
    runtime.refresh(90, params);
    assert_eq!(runtime.remaining_ns(), 5);
    runtime.refresh(130, params);
    assert_eq!(runtime.remaining_ns(), 5);
}

#[test]
fn unbounded_fair_preference_cannot_bypass_arbitration_gates() {
    let unsafe_order = ArbitrationOrder {
        priority_rank: 0,
        fair_rank: 1,
        budget_rank: 2,
        borrow_rank: 3,
    };
    assert!(ArbitrationConfig::new(100, 20, 10, unsafe_order).is_none());
}
