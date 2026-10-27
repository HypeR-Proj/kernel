// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Installed userspace-device routes and hardware-detached MMIO requests.

use super::{Error, InstalledMachine, RuntimeState};
use hyper_vm_models::device::mmio::Request;
use {hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction};

#[derive(Clone, Copy)]
pub(super) struct Region {
    base: u64,
    end: u64,
    device: u64,
}

impl InstalledMachine {
    pub(in crate::vm) fn access_notification(
        &self,
        original: u64,
        operation: u32,
        value: u32,
    ) -> Result<u32, crate::vm::io::Error> {
        self.with_io_update(|id| {
            crate::vm::registry::acquire_binding(id)
                .map_err(|_| crate::vm::io::Error::BadState)?
                .access_notification(original, operation, value)
        })
    }

    pub(in crate::vm) fn configure_notification(
        &self,
        original: u64,
        base: u64,
        flags: u32,
    ) -> Result<(), crate::vm::io::Error> {
        use crate::vm::io::Error;
        self.with_io_update(|id| {
            let end = base.checked_add(4096).ok_or(Error::InvalidArgument)?;
            if !self.mmio_regions.with(|regions| {
                regions
                    .iter()
                    .flatten()
                    .any(|region| region.base <= base && region.end >= end)
            }) {
                return Err(Error::InvalidArgument);
            }
            crate::vm::registry::acquire_binding(id)
                .map_err(|_| Error::BadState)?
                .configure_notification(original, base, flags)
        })
    }

    pub(in crate::vm) fn register_mmio(
        &self,
        base: u64,
        length: u64,
        device: u64,
    ) -> Result<(), Error> {
        if device == 0 || length == 0 {
            return Err(Error::BadState);
        }
        let end = base.checked_add(length).ok_or(Error::BadState)?;
        self.state.with(|state| {
            if !matches!(state, RuntimeState::Installed { .. }) {
                return Err(Error::BadState);
            }
            let id = match state {
                RuntimeState::Installed { id, .. } => *id,
                _ => return Err(Error::BadState),
            };
            // Keep the generation-qualified lease through route publication.
            // The lifecycle lock serializes this with other route installation
            // and first start; a physical window is never an ambient aperture.
            let binding = crate::vm::registry::acquire_binding(id).map_err(|_| Error::BadState)?;
            let permitted = crate::vm::device::supports_userspace_mmio(
                self.configuration.platform_profile,
                base,
                length,
            ) || binding.owns_userspace_assignment_aperture(base, length);
            if !permitted || binding.io_range_conflicts(base, length) {
                return Err(Error::BadState);
            }
            self.mmio_regions.with(|regions| {
                if regions.iter().flatten().any(|region| {
                    region.device == device || (base < region.end && region.base < end)
                }) {
                    return Err(Error::BadState);
                }
                let slot = regions
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .ok_or(Error::BadState)?;
                *slot = Some(Region { base, end, device });
                Ok(())
            })
        })
    }

    #[allow(
        dead_code,
        reason = "selected guest backends opt into deferred userspace MMIO"
    )]
    pub(crate) fn route_mmio(&self, vcpu: u32, access: MmioAccess) -> Option<MmioAction> {
        let base = access.address().get();
        let end = base.checked_add(access.size() as u64)?;
        let device = self.mmio_regions.with(|regions| {
            regions
                .iter()
                .flatten()
                .find(|region| base >= region.base && end <= region.end)
                .map(|region| region.device)
        })?;
        match self.endpoint(vcpu).and_then(|endpoint| {
            endpoint
                .stage_mmio(device, access)
                .map_err(|_| Error::BadState)
        }) {
            Ok(()) => Some(MmioAction::Deferred),
            Err(_) => Some(MmioAction::Stop),
        }
    }

    pub(in crate::vm) fn publish_mmio(&self, vcpu: u32) -> Result<(), Error> {
        self.endpoint(vcpu)?
            .publish_mmio()
            .map_err(|_| Error::BadState)
    }

    pub(in crate::vm) fn pending_mmio(&self, vcpu: u32) -> Result<Option<Request>, Error> {
        self.state.with(|state| {
            if !matches!(state, RuntimeState::Running { .. }) {
                return Err(Error::BadState);
            }
            Ok(self.endpoint(vcpu)?.pending_mmio())
        })
    }

    pub(in crate::vm) fn complete_mmio(
        &self,
        vcpu: u32,
        id: u64,
        action: MmioAction,
    ) -> Result<(), Error> {
        // Completion and stop admission have one lifecycle lock. The runner
        // alone updates architectural state after consuming this result.
        self.state.with(|state| {
            if !matches!(state, RuntimeState::Running { .. }) {
                return Err(Error::BadState);
            }
            self.endpoint(vcpu)?
                .complete_mmio(id, action)
                .map_err(|_| Error::BadState)
        })
    }

    pub(in crate::vm) fn take_mmio_completion(
        &self,
        vcpu: u32,
    ) -> Result<Option<MmioAction>, Error> {
        self.endpoint(vcpu)?
            .take_mmio_completion()
            .map_err(|_| Error::BadState)
    }
}

impl InstalledMachine {
    pub(in crate::vm) fn validate_io_route(
        &self,
        route: &crate::vm::io::Route,
    ) -> Result<(), crate::vm::io::Error> {
        let Some(base) = route.base() else {
            return Ok(());
        };
        self.mmio_regions.with(|regions| {
            if regions.iter().flatten().any(|region| {
                base < region.end
                    && region.base < base + 4096
                    && !(route.deferred_overlay()
                        && region.base <= base
                        && region.end >= base + 4096)
            }) {
                Err(crate::vm::io::Error::Busy)
            } else {
                Ok(())
            }
        })
    }
}
