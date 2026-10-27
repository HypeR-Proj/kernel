// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected guest-platform device service.
//!
//! Reusable register models live under [`hyper_vm_models`]. The selected module owns
//! per-VM model instances, host bindings, and guest-ISA exit decoding. Device
//! policy deliberately remains in the kernel VM service rather than the HAL.

use {crate::object::KernelRef, crate::object::VmDeviceBinding};
use {crate::vm::serial::Route, crate::vm::serial::VirtualSerial};

pub(crate) mod selected;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
mod serial_timeout;

pub use selected::Error;
pub(crate) use selected::VirtualDeviceSet;

/// Virtual-serial authority committed into one VM's device set.
///
/// Production bindings retain a typed reference derived from an explicitly
/// supplied `VirtualSerial` handle. The VM-exit path can therefore exchange
/// bytes without a handle-table lookup or allocation.
pub(crate) struct VirtualSerialBinding {
    serial: KernelRef<VirtualSerial, VmDeviceBinding>,
}

impl VirtualSerialBinding {
    pub(crate) const fn from_virtual_serial(
        serial: KernelRef<VirtualSerial, VmDeviceBinding>,
    ) -> Self {
        Self { serial }
    }

    #[cfg(CONFIG_ARCH_AARCH64)]
    pub(super) fn try_write_byte(&self, byte: u8) -> bool {
        self.serial.object().try_publish_guest_output(byte)
    }

    /// Allocation-free publication into the registered userspace output buffer.
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    pub(super) fn write_byte(&self, byte: u8) {
        self.serial.object().publish_guest_output(byte)
    }

    #[allow(
        dead_code,
        reason = "selected guest UART backends consume input only when they implement receive injection"
    )]
    pub(super) fn pop_guest_input(&self) -> Option<u8> {
        self.serial.object().pop_guest_input()
    }

    pub(super) fn bind(&self, route: Route) {
        self.serial.object().bind(route)
    }

    pub(super) fn disconnect(&self, vm: super::registry::VmId) {
        self.serial.object().disconnect(vm)
    }
}

pub(crate) fn prepare(
    virtual_serial: Option<VirtualSerialBinding>,
) -> Result<VirtualDeviceSet, Error> {
    selected::prepare(virtual_serial)
}

/// Validates guest RAM against the selected immutable platform profile.
pub(crate) fn supports_configuration(profile: u32, memory_base: u64, memory_size: u64) -> bool {
    selected::supports_configuration(profile, memory_base, memory_size)
}

pub(crate) fn supports_userspace_mmio(profile: u32, base: u64, length: u64) -> bool {
    selected::supports_userspace_mmio(profile, base, length)
}

/// Returns the architected timer interrupt used by the selected guest board.
pub(crate) const fn default_timer_interrupt() -> hyper_vm_models::interrupt::VirtualInterruptId {
    selected::default_timer_interrupt()
}

pub(super) fn kick_virtual_serial(route: Route) {
    selected::kick_virtual_serial(route);
}

/// Additional allocation charged before virtual-device construction.
pub(crate) fn dynamic_allocation_bytes() -> usize {
    selected::dynamic_allocation_bytes()
}

pub(crate) const fn timer_count() -> u64 {
    selected::timer_count()
}

/// Retires device callbacks after registry visibility has been cut.
/// Called in normal context without holding registry or device locks.
pub(crate) fn quiesce(devices: &VirtualDeviceSet) -> Result<(), Error> {
    selected::quiesce(devices)
}
