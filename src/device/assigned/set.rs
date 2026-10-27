// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Physical controllers share one VM publication and DMA retirement boundary.

use super::{Assignment, Error, PhysicalDevice};
use crate::vm::registry::VmId;
use hyper_vm_models::exit::MmioAccess;
use {crate::object::KernelRef, crate::object::VmDeviceBinding};

const MAX_DEVICES: usize = hyper::abi::native::HYPER_NATIVE_DEVICE_ASSIGNMENT_MAX_DEVICES as usize;
type DeviceOwner = KernelRef<PhysicalDevice, VmDeviceBinding>;

/// Inline storage is charged as part of the pending/installed VM aggregate.
/// Admission never allocates or destroys a device while holding its VM lock.
pub(crate) struct AssignmentSet {
    entries: [Option<Assignment>; MAX_DEVICES],
}

impl AssignmentSet {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_DEVICES],
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// Failure leaves the caller's ownership intact for rollback outside locks.
    pub(crate) fn insert(&mut self, pending: &mut Option<Assignment>) -> Result<(), Error> {
        let assignment = pending.as_ref().ok_or(Error::BadState)?;
        if self
            .entries
            .iter()
            .flatten()
            .any(|other| assignment.conflicts(other))
        {
            return Err(Error::Busy);
        }
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Busy)?;
        *slot = pending.take();
        Ok(())
    }

    pub(crate) fn mmio_owner(&self, access: MmioAccess) -> Option<(DeviceOwner, usize)> {
        self.entries.iter().flatten().find_map(|assignment| {
            assignment
                .offset(access)
                .map(|offset| (assignment.object(), offset))
        })
    }

    pub(crate) fn has_interrupt(&self, irq: u32) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|assignment| assignment.contains_interrupt(irq))
    }

    pub(crate) fn owns_userspace_aperture(&self, base: u64, length: u64) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|assignment| assignment.owns_userspace_aperture(base, length))
    }

    /// Device transitions and final owner drops must happen outside VM locks.
    pub(crate) fn owners(&self) -> AssignmentOwners {
        AssignmentOwners {
            entries: core::array::from_fn(|index| {
                self.entries[index]
                    .as_ref()
                    .map(|assignment| (assignment.object(), assignment.irq()))
            }),
        }
    }
}

/// A temporary device snapshot, never independent ownership of guest RAM.
/// The caller retains the complete VM until all devices have stopped DMA.
pub(crate) struct AssignmentOwners {
    entries: [Option<(DeviceOwner, u32)>; MAX_DEVICES],
}

impl AssignmentOwners {
    pub(crate) fn dma_streams(&self) -> [Option<u32>; MAX_DEVICES] {
        core::array::from_fn(|index| {
            self.entries[index]
                .as_ref()
                .and_then(|(owner, _)| owner.object().dma_stream())
        })
    }
    pub(crate) fn observe_dma_faults(&self) -> Result<(), hyper_drivers::iommu::smmuv3::Error> {
        let Some(runtime) = super::super::iommu::runtime::owner() else {
            return Err(hyper_drivers::iommu::smmuv3::Error::Unsupported);
        };
        for (owner, _) in self.entries.iter().flatten() {
            if let Some(stream) = owner.object().dma_stream() {
                runtime.observe(stream, &owner.object().events)?;
            }
        }
        Ok(())
    }
    /// A failure may leave earlier controllers active. The installation
    /// transaction must quiesce the entire set before dropping its VM.
    pub(crate) fn activate_for(&self, vm: VmId) -> Result<(), Error> {
        for (device, irq) in self.entries.iter().flatten() {
            device.object().activate_for(vm, *irq)?;
        }
        Ok(())
    }

    /// Attempt every controller, including those after a failure. One reset
    /// timeout quarantines the whole VM, its claims, and all imported pages.
    pub(crate) fn quiesce_all(&self) -> Result<(), Error> {
        let mut result = Ok(());
        for (device, _) in self.entries.iter().flatten() {
            let stopped = device.object().quiesce();
            if result.is_ok() {
                result = stopped;
            }
        }
        result
    }
}
