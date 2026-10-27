// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Linear VMID and stage-2 retirement transactions.

use hyper::mm::RetirementCut;

use super::{Error, GuestAddressSpace, Stage2Identifier};
use crate::vm::residency_state::Stage2AllocationIdentity;

impl GuestAddressSpace {
    /// Cuts off new residency and retains the VMID until CPU acknowledgements.
    ///
    /// The registry owner must already have excluded future VM entry and
    /// reserved the cross-call transport. The returned transaction identifies
    /// every CPU that may retain translations; beginning retirement alone does
    /// not permit page-table or VMID reuse.
    pub(crate) fn begin_retirement(
        &mut self,
        capability: &crate::hal::vm::GuestStage2RetirementCapability,
        topology_count: usize,
    ) -> Result<GuestStage2Retirement, Error> {
        self.ensure_healthy()?;
        if topology_count == 0 || topology_count > hyper::cpu::MAX_CPUS {
            return Err(Error::InvalidCpu);
        }
        let incarnation = self.incarnation()?;
        // Retirement flushes the whole local namespace on sticky CPUs. It must
        // not reacquire a tag after the registry's irreversible retirement cut.
        let _ = capability;
        let cut = self
            .residency
            .begin_retirement(incarnation.translation_epoch())
            .map_err(Error::Residency)?;
        if cut
            .targets()
            .iter()
            .copied()
            .enumerate()
            .any(|(cpu, targeted)| targeted && cpu >= topology_count)
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest retirement targets escape the frozen CPU topology"
            ));
        }

        let previous = core::mem::replace(&mut self.identifier, Stage2Identifier::Poisoned);
        let Stage2Identifier::Active(identifier) = previous else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest retirement lost its active VMID"
            ));
        };
        let retiring = match identifier.begin_retirement() {
            Ok(retiring) => retiring,
            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest VMID retirement could not begin after the residency cut: {error:?}"
            )),
        };
        self.identifier = Stage2Identifier::Retiring(retiring);
        Ok(GuestStage2Retirement {
            cut,
            allocation: incarnation.allocation(),
        })
    }

    /// Completes retirement after all targets acknowledge this exact allocation.
    ///
    /// The caller must finish the transaction's local invalidation requests
    /// before passing it back. Identity mismatches fail-stop; successful
    /// completion retires the residency record and releases the hardware VMID.
    pub(crate) fn finish_retirement(&mut self, retirement: GuestStage2Retirement) {
        let GuestStage2Retirement { cut, allocation } = retirement;
        let identity_matches = match &self.identifier {
            Stage2Identifier::Retiring(identifier) => {
                self.stage2.root_address() == allocation.root()
                    && identifier.generation() == allocation.generation()
            }
            _ => false,
        };
        if !identity_matches {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest retirement completion changed translation identity"
            ));
        }
        if self.residency.finish_retirement(cut).is_err() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest residency retirement completion is inconsistent"
            ));
        }
        let previous = core::mem::replace(&mut self.identifier, Stage2Identifier::Poisoned);
        let Stage2Identifier::Retiring(identifier) = previous else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest retirement lost its retiring VMID"
            ));
        };
        // SAFETY: The residency state is irreversibly retired and the caller
        // obtained exact-generation acknowledgement from every sticky target.
        if unsafe { identifier.complete() }.is_err() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest VMID completion is inconsistent"
            ));
        }
        self.identifier = Stage2Identifier::Retired;
    }
}

#[must_use = "guest retirement must obtain every target acknowledgement"]
pub(crate) struct GuestStage2Retirement {
    cut: RetirementCut<{ hyper::cpu::MAX_CPUS }>,
    allocation: Stage2AllocationIdentity,
}

impl GuestStage2Retirement {
    pub(crate) fn targets(&self) -> &[bool; hyper::cpu::MAX_CPUS] {
        self.cut.targets()
    }

    pub(crate) const fn local_request(&self) -> GuestStage2LocalRequest {
        GuestStage2LocalRequest {
            allocation: self.allocation,
            invalidation: GuestStage2Invalidation::Retired,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GuestStage2LocalRequest {
    pub(super) allocation: Stage2AllocationIdentity,
    pub(super) invalidation: GuestStage2Invalidation,
}

#[derive(Clone, Copy)]
pub(super) enum GuestStage2Invalidation {
    /// The owner is permanently inactive: no leased hardware tag is needed.
    Retired,
    /// The caller retains a maintenance lease through acknowledgement.
    Live(crate::hal::vm::GuestStage2RetirementRequest),
}

pub(crate) fn service_local_retirement(request: GuestStage2LocalRequest) {
    match request.invalidation {
        GuestStage2Invalidation::Live(hardware) => {
            crate::hal::vm::service_guest_stage2_live(hardware)
        }
        GuestStage2Invalidation::Retired => {
            crate::hal::vm::retire_guest_stage2_root_local(request.allocation.root())
        }
    }
    if super::residency::clear_local_observations(request.allocation).is_err() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: guest retirement could not clear local observations"
        ));
    }
}
