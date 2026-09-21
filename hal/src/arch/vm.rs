// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture hardware virtualization mechanisms.
//!
//! Kernel VM policy owns VM publication, vCPU scheduling, demand paging, and
//! exit disposition. This facade selects stage-2 translation, vCPU entry,
//! virtual interrupt, guest timer, and architecture-local exit mechanisms.
//! Linux image formats and boot ABI policy deliberately remain outside it.

use hyper::sync::PublishedOnce;
use hyper::vm::exit::{GuestMemoryFault, MemoryFaultAction};

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
use hyper::vm::exit::{MmioAccess, MmioAction};
#[cfg(CONFIG_ARCH_X86_64)]
use hyper::vm::x86::exit::{PendingInterruptAction, PortIoAction, PortIoExit};

pub use super::imp::{
    VcpuInterruptError, VmInterruptController as InterruptController,
    VmInterruptError as InterruptError,
};

pub use super::imp::{
    GuestValidationError as RegisterValidationError,
    InterruptVirtualizationError as InterruptInitializationError, PreparedInterruptVirtualization,
    Stage2AddressSpace, Stage2Error, VcpuContext, VirtualDeviceInitializationError as DeviceError,
    VirtualInterruptError,
};

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) use super::imp::handle_guest_sync;
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub use super::imp::{GuestSyncAction, GuestSyncExit};

#[cfg(CONFIG_ARCH_AARCH64)]
pub(crate) use super::imp::access_guest_gic;
#[cfg(CONFIG_ARCH_AARCH64)]
pub use super::imp::{GicAccessError, GuestSyncFailure};

#[cfg(CONFIG_ARCH_RISCV64)]
pub use super::imp::UnsupportedGuestExit;
#[cfg(CONFIG_ARCH_RISCV64)]
pub(crate) use super::imp::{access_plic, stopped_guest_wfi_state};

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub use super::imp::{
    GuestAdministrativeStopReason, GuestRunError, GuestRunExit, GuestStage2RetirementRequest,
    GuestSynchronousTerminal, GuestTerminalCause, GuestWaitReason, StoppedDeactivationFailure,
    StoppedGuestRun,
};
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) use super::imp::{
    deactivate_stopped_vcpu_hardware, reconcile_active_interrupts, request_guest_exit,
    update_guest_device_interrupt, update_saved_guest_device_interrupt,
};

/// Checks the hart-local selection; an epoch observation alone does not prove
/// that a stopped-owner detach preserved the selected translation hierarchy.
pub(crate) fn stage2_selection_is_current(address_space: &Stage2AddressSpace) -> bool {
    address_space.is_active_local()
}

pub(crate) fn guest_translation_identifier_bits() -> Result<u8, Stage2Error> {
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        super::imp::guest_translation_identifier_bits()
    }
    #[cfg(not(CONFIG_ARCH_RISCV64))]
    {
        Ok(8)
    }
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn prepare_guest_stage2_retirement(
    address_space: &Stage2AddressSpace,
) -> GuestStage2RetirementRequest {
    address_space.retirement_request()
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn service_guest_stage2_retirement(request: GuestStage2RetirementRequest) {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        super::imp::retire_guest_stage2_local(request)
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        if super::imp::retire_guest_stage2_local(request).is_err() {
            // Never acknowledge retirement while a hardware owner remains live.
            hyper::debug::invariant_failure("vm::service_guest_stage2_retirement invariant");
        }
    }
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn service_guest_stage2_live(request: GuestStage2RetirementRequest) {
    super::imp::synchronize_guest_stage2_local(request);
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn publish_guest_stage2_changes() {
    super::imp::publish_guest_stage2_changes();
}

pub(crate) use super::imp::{
    commit_interrupt_virtualization as commit_interrupts,
    handle_guest_virtual_timer_interrupt as handle_virtual_timer_interrupt,
    handle_virtualization_maintenance_interrupt as handle_maintenance_interrupt,
    initialize_virtual_devices as initialize_devices, interrupt_virtualization_description,
    prepare_interrupt_virtualization as prepare_interrupts,
    prepare_interrupts_for_guest_entry as prepare_interrupts_for_entry,
    quiesce_virtual_interrupt_delivery, validate_vsysreg as validate_register_interface,
};

#[cfg(feature = "kernel-self-test")]
pub(crate) use super::imp::guest_execution_available;

pub(crate) use super::imp::{
    activate_vcpu_hardware, deactivate_vcpu_hardware,
    virtualization_maintenance_pending as maintenance_interrupt_pending,
};

#[cfg(CONFIG_ARCH_AARCH64)]
pub(crate) use super::imp::inject_timer_for_validation;

/// Immutable kernel callbacks reachable from architecture-owned guest entry.
///
/// Every callback consumes an owned, fixed-width event. Raw exception frames,
/// VMCS/VMCB state, and architecture completion metadata remain below this
/// facade. The target-specific members exist only where their semantics are
/// real; there is deliberately no universal guest-exit enum.
#[derive(Clone, Copy)]
pub struct ExitServices {
    memory_fault: fn(GuestMemoryFault) -> MemoryFaultAction,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    mmio: fn(MmioAccess) -> MmioAction,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    guest_sync: fn(GuestSyncExit) -> GuestSyncAction,
    #[cfg(CONFIG_ARCH_X86_64)]
    port_io: fn(PortIoExit) -> PortIoAction,
    #[cfg(CONFIG_ARCH_X86_64)]
    pending_interrupt: fn(bool) -> PendingInterruptAction,
}

impl ExitServices {
    #[cfg(CONFIG_ARCH_AARCH64)]
    pub const fn aarch64(
        memory_fault: fn(GuestMemoryFault) -> MemoryFaultAction,
        mmio: fn(MmioAccess) -> MmioAction,
        guest_sync: fn(GuestSyncExit) -> GuestSyncAction,
    ) -> Self {
        Self {
            memory_fault,
            mmio,
            guest_sync,
        }
    }

    #[cfg(CONFIG_ARCH_RISCV64)]
    pub const fn riscv64(
        memory_fault: fn(GuestMemoryFault) -> MemoryFaultAction,
        mmio: fn(MmioAccess) -> MmioAction,
        guest_sync: fn(GuestSyncExit) -> GuestSyncAction,
    ) -> Self {
        Self {
            memory_fault,
            mmio,
            guest_sync,
        }
    }

    #[cfg(CONFIG_ARCH_X86_64)]
    pub const fn x86_64(
        memory_fault: fn(GuestMemoryFault) -> MemoryFaultAction,
        port_io: fn(PortIoExit) -> PortIoAction,
        pending_interrupt: fn(bool) -> PendingInterruptAction,
    ) -> Self {
        Self {
            memory_fault,
            port_io,
            pending_interrupt,
        }
    }
}

static EXIT_SERVICES: PublishedOnce<ExitServices> = PublishedOnce::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitServiceError {
    AlreadyInstalled,
}

/// Proof that immutable VM-exit callbacks have been published.
///
/// This is intentionally weaker than complete VM-entry readiness: device,
/// interrupt, and timer initialization still follow service publication.
#[must_use]
pub struct ExitServicesReady {
    _private: (),
}

pub(crate) fn install_exit_services(
    services: ExitServices,
) -> Result<ExitServicesReady, ExitServiceError> {
    EXIT_SERVICES
        .publish(services)
        .map_err(|_| ExitServiceError::AlreadyInstalled)?;
    Ok(ExitServicesReady { _private: () })
}

fn exit_services() -> ExitServices {
    let Some(services) = EXIT_SERVICES.get().copied() else {
        hyper::debug::invariant_failure("vm::exit_services invariant")
    };
    services
}

pub(crate) fn dispatch_memory_fault(fault: GuestMemoryFault) -> MemoryFaultAction {
    (exit_services().memory_fault)(fault)
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn dispatch_mmio(access: MmioAccess) -> MmioAction {
    (exit_services().mmio)(access)
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(crate) fn dispatch_guest_sync(exit: GuestSyncExit) -> GuestSyncAction {
    (exit_services().guest_sync)(exit)
}

#[cfg(CONFIG_ARCH_X86_64)]
pub(crate) fn dispatch_port_io(exit: PortIoExit) -> PortIoAction {
    (exit_services().port_io)(exit)
}

#[cfg(CONFIG_ARCH_X86_64)]
pub(crate) fn query_pending_interrupt(timer_pending: bool) -> PendingInterruptAction {
    (exit_services().pending_interrupt)(timer_pending)
}

#[cfg(CONFIG_ARCH_RISCV64)]
pub(crate) use super::imp::riscv_guest_baseline_available;

/// Whether this platform has an implemented guest interrupt backend.
pub(crate) fn platform_supported() -> bool {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        super::imp::guest_interrupts_supported()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        true
    }
}

pub(crate) fn guest_gic_version() -> u32 {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        super::imp::guest_gic_version()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        0
    }
}

pub(crate) const fn maximum_guest_vcpus() -> u32 {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        super::imp::maximum_guest_vcpus()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        1
    }
}
