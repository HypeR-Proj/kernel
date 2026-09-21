// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture CPU identity, lifecycle, and power mechanisms.
//!
//! Kernel CPU policy assigns logical indices and chooses when processors
//! start, idle, or stop. This facade validates architecture-produced indices
//! and keeps firmware-visible identifiers distinct from logical CPU indices.

use hyper::cpu::CpuIndex;
use hyper::hal::cpu_power::{CpuHardwareId, ResumeAddress};

pub use super::imp::{
    ArchitectureCpuPower as PowerController, CpuPowerError as PowerError, SecondaryBootParameters,
};

#[inline]
pub(crate) fn current_index() -> Option<CpuIndex> {
    CpuIndex::new(super::imp::current_cpu_index())
}

pub(crate) fn current_hardware_id() -> CpuHardwareId {
    CpuHardwareId::new(super::imp::current_hardware_id())
}

pub(crate) fn secondary_entry_address(
    image_physical_start: u64,
    kernel_virtual_base: u64,
) -> Option<ResumeAddress> {
    super::imp::secondary_entry_physical(image_physical_start, kernel_virtual_base)
        .map(ResumeAddress::new)
}

pub(crate) fn register_secondary(index: CpuIndex, hardware_id: CpuHardwareId) -> bool {
    super::imp::register_secondary_hardware_id(index.get(), hardware_id.get())
}

pub(crate) use super::imp::{
    halt, initialize_cpu_power as initialize_power, mark_current_cpu_online as mark_current_online,
    prepare_primary_cpu_admission as prepare_primary_admission,
    secondary_cpu_is_compatible as secondary_is_compatible, send_event, wait_for_interrupt_masked,
};

pub(crate) fn prepare_secondary_entry() {
    super::imp::prepare_secondary_entry();
}
