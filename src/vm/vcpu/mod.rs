// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! vCPU scheduler runner and local hardware-transition facade.

mod mmio;
mod power;

mod execution;
mod interrupt_gate;
mod lifecycle;
mod runner;
mod transition;

use super::{active_vcpu, memory, registry, timer};

pub(crate) use execution::VcpuExecution;
pub(crate) use interrupt_gate::wait_for_interrupt_gate;
pub(crate) use lifecycle::{DetachedStopError, complete_detached_stop_if_requested};
pub(super) use runner::create_thread;
pub(crate) use transition::HardwareTransitionError;
pub(crate) use transition::{activate, deactivate};
pub(crate) use transition::{
    current_administrative_stop_requested, current_interrupt_reconcile_pending,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconcileObservationError {
    Active(active_vcpu::Error),
    Registry(registry::Error),
    Scheduler(crate::task::scheduler::Error),
}
