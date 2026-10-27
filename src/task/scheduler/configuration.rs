// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Image defaults, separate from algorithm and context ownership mechanisms.

use super::policy::{
    BudgetParameters, FairBackend, SchedulingParameters, SchedulingPolicy, SchedulingRole,
    ThreadPriority, ThreadWeight,
};

pub(crate) const PERIOD_NS: u64 = hyper::config::SCHED_PERIOD_MS as u64 * 1_000_000;

pub(super) fn arbitration() -> super::algorithms::arbitration::ArbitrationConfig {
    use super::algorithms::arbitration::{ArbitrationConfig, ArbitrationOrder};
    ArbitrationConfig::new(
        PERIOD_NS,
        hyper::config::SCHED_PRIORITY_ALLOWANCE_MS as u64 * 1_000_000,
        hyper::config::SCHED_PROTECTED_MIN_MS as u64 * 1_000_000,
        ArbitrationOrder::DEFAULT,
    )
    .unwrap_or_else(|| hyper::debug::invariant_failure("invalid CPU arbitration configuration"))
    .with_prefer_fair(cfg!(CONFIG_SCHED_PREFER_NATIVE))
}

pub(crate) fn default_parameters(policy: SchedulingPolicy) -> SchedulingParameters {
    match policy {
        SchedulingPolicy::Idle => SchedulingParameters::idle(),
        SchedulingPolicy::Fifo { priority } => {
            SchedulingParameters::fifo(priority, ThreadPriority::NORMAL, SchedulingRole::Ordinary)
        }
        SchedulingPolicy::Budget => default_vcpu_parameters(),
        SchedulingPolicy::Fair => SchedulingParameters::fair(
            ThreadWeight::DEFAULT,
            hyper::config::SCHED_FAIR_QUANTUM_MS as u64 * 1_000_000,
            ThreadPriority::NORMAL,
            SchedulingRole::Protected,
            if cfg!(CONFIG_SCHED_FAIR_RR) {
                FairBackend::RoundRobin
            } else {
                FairBackend::Stride
            },
        )
        .unwrap_or_else(|| hyper::debug::invariant_failure("invalid default Fair configuration")),
    }
}

pub(crate) fn default_vcpu_parameters() -> SchedulingParameters {
    let budget = BudgetParameters::new(
        hyper::config::SCHED_VCPU_BUDGET_MS as u64 * 1_000_000,
        PERIOD_NS,
        hyper::config::SCHED_VCPU_BURST_MS as u64 * 1_000_000,
    )
    .unwrap_or_else(|| hyper::debug::invariant_failure("invalid default Budget configuration"));
    SchedulingParameters::budget(budget, ThreadPriority::NORMAL, SchedulingRole::Ordinary)
}

/// Background kernel maintenance shares the protected pool with applications.
/// Its larger weight improves progress without an unbounded strict class.
pub(super) fn deferred_parameters() -> SchedulingParameters {
    let ordinary = default_parameters(SchedulingPolicy::Fair);
    SchedulingParameters::fair(
        ThreadWeight::new(32).unwrap_or(ThreadWeight::DEFAULT),
        ordinary.quantum_ns(),
        ordinary.band(),
        ordinary.role(),
        ordinary.fair_backend(),
    )
    .unwrap_or_else(|| hyper::debug::invariant_failure("invalid deferred scheduling parameters"))
}
