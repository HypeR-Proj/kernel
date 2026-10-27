// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Virtual-machine initialization and execution policy.
//!
//! This subsystem owns hardware-virtualization validation, virtual interrupt
//! activation, guest-visible devices, VM publication, and vCPU orchestration.

pub(crate) mod serial;

pub(crate) mod active_vcpu;
mod address_space_state;
pub(crate) mod device;
mod diagnostics;
pub(crate) use diagnostics::UnhandledMmioReport;
mod endpoint;
mod endpoint_state;
mod endpoint_wait;
mod installed;
pub(crate) mod io;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use endpoint::{WaitSelfTestError, run_wait_self_test};
pub(crate) mod lifecycle;
pub mod memory;
pub(crate) mod objects;
mod reconcile;
pub(crate) mod registry;
mod residency_state;
mod retirement_observation;
mod run_admission;
pub(crate) mod service;
mod timer;
pub(crate) use timer::recover_source as recover_timer_source;
pub(crate) mod vcpu;

use hyper::sync::PublishedOnce;

static ENTRY_READY: PublishedOnce<crate::hal::vm::VmEntryReady> = PublishedOnce::new();

pub use crate::hal::vm::{
    InterruptController as VmInterruptController, InterruptError as VmInterruptError,
};
pub use registry::VmId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    Devices(crate::hal::vm::DeviceError),
    Interrupts(crate::hal::vm::InterruptInitializationError),
    EntryServices(crate::hal::vm::ExitServiceError),
    EntryReadyAlreadyPublished,
    Registers(crate::hal::vm::RegisterValidationError),
    Timer(timer::Error),
    TimerValidation(timer::ValidationError),
}

/// Initializes hardware virtualization and guest-visible platform devices.
pub(crate) fn initialize(boot: &crate::boot::Initialization) -> Result<(), InitializationError> {
    if !crate::hal::vm::platform_supported() {
        crate::pr_info!(
            "HypeR: guest VM backend unavailable on this interrupt controller; Native applications remain supported"
        );
        return Ok(());
    }
    let exit_services = crate::hal::vm::install_exit_services(crate::entry::vmexit::services())
        .map_err(InitializationError::EntryServices)?;
    crate::hal::vm::validate_register_interface().map_err(InitializationError::Registers)?;
    // Prepared physical mappings must remain non-deliverable until every VM
    // dependency below is published. Quiesce virtual delivery before exposing
    // their disabled handler records to the IRQ registry.
    crate::hal::vm::quiesce_virtual_interrupt_delivery();
    let interrupts = boot.interrupts();
    let guest_timer = boot.timer().guest_timer;
    let binding = timer::prepare(
        guest_timer,
        interrupts.root_domain,
        interrupts.maintenance_interrupt,
    )
    .map_err(InitializationError::Timer)?;
    if let Some(interrupt) = binding.host_interrupt() {
        crate::pr_info!(
            "HypeR: guest architectural timer mapped to host VIRQ {}",
            interrupt.get()
        );
    }
    if let Err(error) =
        crate::hal::vm::initialize_devices(guest_timer.interrupt, binding.host_interrupt())
    {
        binding.rollback();
        return Err(InitializationError::Devices(error));
    }
    let prepared_interrupts = match crate::hal::vm::prepare_interrupts(binding.host_interrupt()) {
        Ok(prepared) => prepared,
        Err(error) => {
            binding.rollback();
            return Err(InitializationError::Interrupts(error));
        }
    };
    match timer::validate_hardware(guest_timer.interrupt, &exit_services, &prepared_interrupts) {
        Ok(true) => crate::pr_info!("HypeR: virtual architected timer injection validated"),
        Ok(false) => {}
        Err(error) => {
            binding.rollback();
            return Err(InitializationError::TimerValidation(error));
        }
    }
    if let Err(error) = crate::hal::vm::commit_interrupts(prepared_interrupts) {
        binding.rollback();
        return Err(InitializationError::Interrupts(error));
    }
    if let (Some(description), Some(maintenance)) = (
        crate::hal::vm::interrupt_virtualization_description(),
        binding.maintenance_interrupt(),
    ) {
        crate::pr_info!(
            "HypeR: vGICv{} active with {} LRs, {} priority bits, {} preemption bits, {} INTID bits, maintenance VIRQ {}",
            crate::hal::vm::guest_gic_version(),
            description.list_registers,
            description.priority_bits,
            description.preemption_bits,
            description.interrupt_id_bits,
            maintenance.get(),
        );
    }
    binding.activate().map_err(InitializationError::Timer)?;
    // SAFETY: Register validation, virtual-device initialization, hardware
    // timer validation, and interrupt virtualization all completed above.
    // `binding.activate` committed the host routes and consumed their rollback
    // path. Kernel boot owns the only VM initialization transaction.
    let entry_ready = unsafe { crate::hal::vm::commit_entry_initialization(exit_services) };
    ENTRY_READY
        .publish(entry_ready)
        .map_err(|_| InitializationError::EntryReadyAlreadyPublished)?;
    crate::pr_info!("HypeR: guest synchronous trap and vSysReg emulation validated");
    Ok(())
}

pub(crate) fn entry_ready() -> Option<crate::hal::vm::VmEntryReady> {
    ENTRY_READY.get().copied()
}

pub(crate) use memory::live_service::{
    create as create_guest_mapping, release as release_guest_mapping,
};
