// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected CPU identity, lifecycle, and low-power capabilities.
//!
//! Kernel policy assigns logical CPU indices and decides when a processor
//! starts, idles, or stops. This facade selects the machine implementation
//! while preserving typed logical and firmware-visible CPU identities.

use hyper::cpu::CpuIndex;
use hyper::hal::cpu_power::{CpuHardwareId, ResumeAddress};
use hyper::platform::CpuPowerInfo;

pub use crate::arch::cpu::{PowerController, PowerError, SecondaryBootParameters};

pub fn secondary_boot_parameters(
    memory: super::memory::SecondaryActivationContext,
    physical_stack_top: u64,
    virtual_stack_top: u64,
    cpu_index: CpuIndex,
    entry: extern "C" fn(usize) -> !,
) -> SecondaryBootParameters {
    SecondaryBootParameters::new(
        memory.into_backend(),
        physical_stack_top,
        virtual_stack_top,
        cpu_index.get(),
        entry,
    )
}

#[inline]
pub fn initialize_power(info: CpuPowerInfo) -> Result<PowerController, PowerError> {
    crate::arch::cpu::initialize_power(info)
}

#[inline]
pub fn secondary_is_compatible() -> bool {
    crate::arch::cpu::secondary_is_compatible()
}

/// Discovers primary-hart execution capabilities before SMP admission.
pub fn prepare_primary_admission() -> bool {
    crate::arch::cpu::prepare_primary_admission()
}

#[inline]
pub fn current_index() -> Option<CpuIndex> {
    crate::arch::cpu::current_index()
}

#[inline]
pub fn current_hardware_id() -> CpuHardwareId {
    crate::arch::cpu::current_hardware_id()
}

#[inline]
pub fn secondary_entry_address(
    image_physical_start: u64,
    kernel_virtual_base: u64,
) -> Option<ResumeAddress> {
    crate::arch::cpu::secondary_entry_address(image_physical_start, kernel_virtual_base)
}

#[inline]
pub fn register_secondary(index: CpuIndex, hardware_id: CpuHardwareId) -> bool {
    crate::arch::cpu::register_secondary(index, hardware_id)
}

#[inline]
pub fn mark_current_online() {
    crate::arch::cpu::mark_current_online();
}

#[inline]
pub fn send_event() {
    crate::arch::cpu::send_event();
}

/// Waits after scheduler work was checked with local interrupts masked.
///
/// The selected architecture must close its check-to-sleep race and return
/// with interrupts still masked so the caller can restore exact guard state.
#[inline]
pub fn wait_for_interrupt_masked() {
    crate::arch::cpu::wait_for_interrupt_masked();
}

#[inline]
pub fn halt() -> ! {
    crate::arch::cpu::halt()
}

pub fn prepare_secondary_entry() {
    crate::arch::cpu::prepare_secondary_entry();
}
