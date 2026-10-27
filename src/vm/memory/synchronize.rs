// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A pre-reserved, acknowledged live translation invalidation transaction.

use super::{Error, GuestStage2LocalRequest};
use crate::irq::cross_call::GuestStage2Transaction;
use crate::vm::registry::VmBinding;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SynchronizationError {
    Unsupported,
    TopologyUnavailable,
    TransportBusy,
    Memory(Error),
}

#[must_use = "after clearing leaves, execute before releasing any old backing"]
pub(crate) struct LiveSynchronization<'a> {
    // The borrow prevents the binding (and hence root/VMID ownership) from
    // disappearing during a cross-call. Registry retirement shares the same
    // transport reservation, so it cannot retire the identity concurrently.
    _binding: &'a VmBinding,
    _identifier: super::Stage2IdentifierLease,
    capability: crate::hal::vm::GuestStage2RetirementCapability,
    transaction: GuestStage2Transaction,
    request: GuestStage2LocalRequest,
    count: usize,
}

pub(crate) fn prepare_live_synchronization(
    binding: &VmBinding,
) -> Result<LiveSynchronization<'_>, SynchronizationError> {
    let capability = crate::hal::vm::try_guest_stage2_retirement()
        .map_err(|_| SynchronizationError::Unsupported)?;
    let topology =
        crate::cpu::frozen_topology().ok_or(SynchronizationError::TopologyUnavailable)?;
    let count = topology.count();
    if count == 0 || count > hyper::cpu::MAX_CPUS || count != crate::cpu::online_cpu_count() {
        return Err(SynchronizationError::TopologyUnavailable);
    }
    let transaction =
        GuestStage2Transaction::try_acquire().map_err(|()| SynchronizationError::TransportBusy)?;
    let (request, identifier) = binding
        .with_address_space(|space| {
            space.ensure_healthy()?;
            let identifier = space
                .active_identifier()?
                .acquire()
                .map_err(Error::Identifier)?;
            // SAFETY: This transaction pins the tag until all remote acknowledgements.
            unsafe { space.stage2.bind_identifier(identifier.value()) }.map_err(Error::Stage2)?;
            Ok((
                GuestStage2LocalRequest {
                    allocation: space.incarnation()?.allocation(),
                    invalidation: super::retirement::GuestStage2Invalidation::Live(
                        crate::hal::vm::prepare_guest_stage2_retirement(&capability, &space.stage2),
                    ),
                },
                identifier,
            ))
        })
        .map_err(SynchronizationError::Memory)?;
    Ok(LiveSynchronization {
        _binding: binding,
        _identifier: identifier,
        capability,
        transaction,
        request,
        count,
    })
}

impl LiveSynchronization<'_> {
    /// Complete after revoking software lookup and clearing the retained leaves.
    /// No address-space lock may remain held while a remote CPU acknowledges.
    /// Keep this guard until detached ownership has been removed from the pool;
    /// another retirement must not publish unacknowledged pages into that pool.
    pub(crate) fn execute(&mut self) {
        crate::hal::vm::publish_guest_stage2_changes(&self.capability);
        let mut targets = [false; hyper::cpu::MAX_CPUS];
        targets[..self.count].fill(true);
        // Target all frozen CPUs, not a racy snapshot of current vCPU owners.
        // A concurrent entry can only observe the cleared tree; its former
        // translation is invalidated before this operation returns.
        let outcome = self.transaction.execute(self.request, self.count, &targets);
        if outcome.rejected_cpu.is_some() || outcome.ambiguous_cpu.is_some() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: live guest invalidation was not acknowledged"
            ));
        }
    }
}
