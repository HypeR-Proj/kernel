// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{Error, PhysicalDevice, State};
use hyper_vm_models::exit::MmioAccess;
use {crate::object::KernelRef, crate::object::VmDeviceBinding};

pub(crate) struct Assignment {
    object: KernelRef<PhysicalDevice, VmDeviceBinding>,
    base: u64,
    irq: u32,
}
impl Assignment {
    pub(crate) fn new(
        object: KernelRef<PhysicalDevice, VmDeviceBinding>,
        base: u64,
        irq: u32,
    ) -> Result<Self, Error> {
        if !crate::hal::vm::supports_guest_device_assignment() {
            return Err(Error::Unsupported);
        }
        if !super::super::model::assignment_aperture(
            base,
            object.object().claim.hardware.profile.aperture(),
        ) || !super::super::model::valid_guest_interrupt_range(
            irq,
            object.object().guest_interrupt_count(),
        ) {
            return Err(Error::InvalidArgument);
        }
        object.object().attach(base, irq)?;
        Ok(Self { object, base, irq })
    }
    pub(crate) fn object(&self) -> KernelRef<PhysicalDevice, VmDeviceBinding> {
        self.object.clone()
    }
    pub(crate) const fn irq(&self) -> u32 {
        self.irq
    }
    pub(in crate::device::assigned) fn contains_interrupt(&self, irq: u32) -> bool {
        irq >= self.irq && irq - self.irq < self.object.object().guest_interrupt_count()
    }
    pub(in crate::device::assigned) fn conflicts(&self, other: &Self) -> bool {
        super::super::model::assignments_conflict(
            (
                self.base,
                self.object.object().claim.hardware.profile.aperture(),
                self.irq,
                self.object.object().guest_interrupt_count(),
            ),
            (
                other.base,
                other.object.object().claim.hardware.profile.aperture(),
                other.irq,
                other.object.object().guest_interrupt_count(),
            ),
        )
    }
    /// Only the exact aperture owned by this userspace assignment can be
    /// delegated to an installed Native MMIO handler.
    pub(crate) fn owns_userspace_aperture(&self, base: u64, length: u64) -> bool {
        super::super::model::owns_userspace_aperture(
            matches!(
                self.object.object().claim.hardware.profile,
                super::super::Profile::Userspace
            ),
            self.base,
            base,
            length,
        )
    }
    pub(crate) fn offset(&self, access: MmioAccess) -> Option<usize> {
        if matches!(
            self.object.object().claim.hardware.profile,
            super::super::Profile::Userspace
        ) {
            return None;
        }
        let offset = access.address().get().checked_sub(self.base)?;
        if offset >= self.object.object().claim.hardware.profile.aperture() {
            return None;
        }
        Some(offset as usize)
    }
}
impl Drop for Assignment {
    fn drop(&mut self) {
        self.object.object().state.with(|state| match state {
            State::Attached => *state = State::Claimed,
            State::Claimed | State::Retired => {}
            State::Active(_) | State::Quarantined => crate::diagnostics::crash::fatal(
                format_args!("active physical assignment dropped without quiescence"),
            ),
        });
    }
}
