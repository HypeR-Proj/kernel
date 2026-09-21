// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

mod barrier;
mod cache;
mod context;
mod exception;
mod guest;
mod interrupt_controller;
mod interrupts;
mod isa;
mod kaslr;
mod memory;
mod platform;
#[allow(dead_code)]
pub mod registers;
mod sbi;
mod smp;
mod stage2;
mod timer;
mod user_entry;
mod user_machine;
mod vm_interrupt;
mod vm_vcpu;

use core::arch::asm;

pub type InterruptVirtualizationError = core::convert::Infallible;

pub use barrier::Riscv64Barrier as ArchitectureBarrier;
pub use cache::Riscv64Cache as ArchitectureCache;
pub use context::{
    GuestAdministrativeStopReason, GuestRunError, GuestRunExit, GuestSynchronousTerminal,
    GuestTerminalCause, GuestWaitReason, StoppedGuestRun,
};
pub use context::{ThreadContext, VcpuContext, VirtualInterruptError};
pub(crate) use context::{reset_stack_and_enter, switch_thread_context};
pub use exception::CrashContext;
pub use exception::ValidationError as RuntimeVectorError;
pub(crate) use exception::{
    bootstrap_stack_bounds, capture_crash_context, install_exception_stacks,
    install_local_runtime_vectors, install_runtime_vectors, run_on_emergency_stack,
    validate_local_runtime_vectors, validate_runtime_vectors,
};
pub use guest::ValidationError as GuestValidationError;
pub(crate) use guest::handle_guest_sync;
pub use guest::{GuestSyncAction, GuestSyncExit, UnsupportedGuestExit};
pub use interrupt_controller::{
    Error as InterruptControllerError,
    Riscv64InterruptController as ArchitectureInterruptController,
};
pub use interrupts::LocalInterruptMask;
pub(crate) use interrupts::{
    disable_all as disable_all_interrupts, enable_irq as enable_local_irq,
    irq_enabled as local_irq_enabled, mask_irq as mask_local_irq,
};
pub use kaslr::Error as KaslrError;
pub(crate) use kaslr::select as select_kaslr_layout;
#[cfg(CONFIG_CRASH_CONSOLE)]
pub use memory::inspect_mapping as inspect_stage1_mapping;
pub use memory::{
    ActivationContext, Error as MemoryError, PreparedAddressSpace,
    Riscv64AddressTranslation as ArchitectureAddressTranslation, SecondaryActivationContext,
    StackMapping,
};
pub(crate) use platform::decode_platform_interrupt;
pub use platform::{
    Error as PlatformDiscoveryError, EssentialDeviceDiscovery, EssentialPlatformInfo,
};
pub use sbi::{Error as CpuPowerError, Sbi as ArchitectureCpuPower};
pub use smp::SecondaryBootParameters;
pub(crate) use smp::{
    current_cpu_index, current_hardware_id, notify_reschedule, secondary_entry_physical, send_event,
};
pub use stage2::GuestStage2RetirementRequest;
pub use stage2::{Error as Stage2Error, Stage2AddressSpace};
pub(crate) use stage2::{
    identifier_bits as guest_translation_identifier_bits,
    publish_changes as publish_guest_stage2_changes, retire_local as retire_guest_stage2_local,
    synchronize_local as synchronize_guest_stage2_local,
};
pub use timer::{
    Error as TimerError, RiscvTimeCounter as ArchitectureCounter,
    SupervisorTimer as ArchitectureTimer,
};
pub use user_entry::Error as UserEntryError;
pub(crate) use user_entry::run_user;
pub(crate) use user_entry::{
    CompletionFailure as UserCompletionFailure, ReturnCapability as UserReturnCapability,
    UserContext, UserExit,
};
#[cfg(feature = "kernel-self-test")]
pub(crate) use user_entry::{
    direct_native_call_count_for_test, native_fault_test_programs_for_test,
    native_register_test_program_for_test,
};
pub use user_machine::{ContractError as UserMachineContractError, Error as UserAddressSpaceError};
pub(crate) use user_machine::{
    LocalActivation as UserLocalActivation, LocalIdentity as UserLocalIdentity,
    LocalOperation as UserLocalOperation, LocalRequest as UserLocalRequest,
    MappingPage as UserMappingPage, PreparedAddressSpace as PreparedUserAddressSpace,
};
pub(crate) use user_machine::{
    activate_local as activate_user_local, application_address_limit, assert_kernel_access,
    copy_from_exposed, copy_to_exposed, deactivate_local as deactivate_user_local,
    identifier_bits as user_translation_identifier_bits,
    local_identity_is_active as user_local_identity_is_active,
    prepare_host as prepare_host_user_address_space,
    service_local_request as service_user_local_request, user_address_limit,
};
pub use vm_interrupt::{Error as VmInterruptError, VmInterruptController};
pub use vm_vcpu::Error as VcpuInterruptError;
pub use vm_vcpu::StoppedDeactivationFailure;
pub(crate) use vm_vcpu::{
    access_plic, deactivate_stopped as deactivate_stopped_vcpu_hardware,
    reconcile_active_interrupts, request_guest_exit, stopped_guest_wfi_state,
    update_guest_device_interrupt, update_saved_guest_device_interrupt,
};
pub(crate) use vm_vcpu::{
    activate as activate_vcpu_hardware, deactivate as deactivate_vcpu_hardware,
    handle_maintenance_interrupt as handle_virtualization_maintenance_interrupt,
    handle_virtual_timer_interrupt as handle_guest_virtual_timer_interrupt,
    maintenance_interrupt_pending as virtualization_maintenance_pending,
    quiesce_virtual_interrupt_delivery,
};

pub type VirtualDeviceInitializationError = core::convert::Infallible;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicCapabilities;

impl AtomicCapabilities {
    pub const fn backend_name(self) -> &'static str {
        "RV64A AMO/LR-SC"
    }
}
pub const fn atomic_capabilities() -> AtomicCapabilities {
    AtomicCapabilities
}

pub const fn service_stage1_tlb_shootdown() -> bool {
    true
}

/// Sv39 permissions directly encode the kernel's RX/R/XN split; there is no
/// AArch64-style global WXN control to enable on each hart.
pub fn enable_local_memory_protection() {}

pub const fn local_memory_protection_enabled() -> bool {
    true
}

pub fn initialize_cpu_power(
    info: hyper::platform::CpuPowerInfo,
) -> Result<ArchitectureCpuPower, CpuPowerError> {
    match info {
        hyper::platform::CpuPowerInfo::Sbi(_) => sbi::bind(),
        hyper::platform::CpuPowerInfo::Psci(_) | hyper::platform::CpuPowerInfo::X86Apic(_) => {
            Err(CpuPowerError::NotSupported)
        }
    }
}

pub fn prepare_primary_cpu_admission() -> bool {
    platform::guest_baseline_available()
        && user_machine::discover_local()
        && stage2::discover_local()
        && vm_vcpu::discover_local_timer()
}

pub fn secondary_cpu_is_compatible() -> bool {
    prepare_primary_cpu_admission()
}
pub fn register_secondary_hardware_id(cpu_index: usize, hardware_id: u64) -> bool {
    smp::register_hart(cpu_index, hardware_id)
}
pub fn mark_current_cpu_online() {
    smp::mark_current_hart_online();
}
pub fn prepare_timekeeping(platform: &EssentialPlatformInfo) -> Result<(), TimerError> {
    timer::set_frequency(platform.timebase_frequency)
}
pub fn prepare_cache(
    platform: &EssentialPlatformInfo,
) -> Result<(), hyper::hal::cache::CacheError> {
    cache::initialize(platform.cache_block_size)
}

pub fn decode_kernel_timer(
    info: hyper::platform::TimerInfo,
) -> Result<crate::arch::time::Description, crate::arch::time::DescriptionError> {
    if info.kind != hyper::platform::TimerKind::RiscvSupervisor
        || info.hypervisor_physical.trigger != hyper::platform::PlatformInterruptTrigger::Level
    {
        return Err(crate::arch::time::DescriptionError::UnsupportedTimer);
    }
    Ok(crate::arch::time::Description {
        hardware: info.hypervisor_physical,
        guest_virtual_interrupt: hyper::hal::interrupt::InterruptId::new(5),
        map_guest_virtual_interrupt: false,
    })
}

pub fn initialize_virtual_devices(
    _timer_interrupt: hyper::hal::interrupt::InterruptId,
    _host_timer_interrupt: Option<hyper::hal::interrupt::HostInterruptBinding>,
) -> Result<(), VirtualDeviceInitializationError> {
    Ok(())
}

/// The returning guest anchor must be fully published before IRQs become
/// deliverable. Assembly sets SPIE and SRET enables delivery atomically with
/// entry into the guest; host execution remains masked throughout preparation.
pub const fn prepare_interrupts_for_guest_entry() {}

/// Builds the permanent HS address space while translation is disabled.
///
/// # Safety
///
/// Allocator results must be directly writable, uniquely owned physical RAM.
pub unsafe fn prepare_address_space(
    allocator: &mut hyper::mm::BootAllocator,
    platform: &hyper::platform::PlatformInfo,
    image: hyper::hal::memory::KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, MemoryError> {
    // SAFETY: This entry point runs in the identity-addressed boot phase.
    unsafe { stage2::prepare_discovery() };
    // SAFETY: This function forwards its directly writable allocator contract.
    unsafe { memory::prepare(allocator, platform, image, kernel_base) }
}

unsafe extern "C" {
    fn riscv64_activate_final_address_space(root: u64, kernel_base: u64, stack_top: u64) -> !;
}

/// Activates a prepared address space and switches to its permanent stack.
///
/// # Safety
///
/// `context` must come from the retained prepared address space; no live
/// reference may depend on a mapping removed by the transition.
pub unsafe fn activate_memory(context: ActivationContext) -> ! {
    // SAFETY: The caller guarantees this activation context remains backed and live.
    unsafe {
        riscv64_activate_final_address_space(
            context.root.get(),
            context.kernel_base,
            context.stack_top.get(),
        )
    }
}

pub fn halt() -> ! {
    loop {
        // SAFETY: WFI is valid in HS mode. A memory clobber prevents state from
        // moving across an interrupt handler that resumes this hart.
        unsafe { asm!("wfi", options(nostack)) }
    }
}
/// Waits for a locally enabled interrupt while SSTATUS.SIE remains clear.
///
/// RISC-V requires WFI to resume for a locally enabled pending interrupt
/// regardless of the global SIE bit. The interrupt remains pending until the
/// caller restores its exact saved mask state.
pub fn wait_for_interrupt_masked() {
    // SAFETY: WFI is valid in HS mode and remains a compiler memory boundary.
    // A locally enabled pending source resumes WFI even while SSTATUS.SIE is
    // clear, so the outer mask closes the queue-check-to-sleep race without an
    // interrupt-enabled window before the wait instruction.
    unsafe { asm!("wfi", options(nostack)) }
}

pub const fn port_io() -> Option<hyper::hal::io::PortIo> {
    None
}

pub const fn crash_stop_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    None
}
pub const fn reschedule_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    None
}
pub const fn kernel_rpc_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    None
}
pub fn arm_kernel_rpc_source() {
    interrupts::enable_software_interrupt_source();
}
pub fn notify_kernel_rpc(cpu: hyper::cpu::CpuIndex, reasons: u8) -> bool {
    if !smp::publish_kernel_rpc(cpu, reasons) {
        return true;
    }
    notify_reschedule(cpu)
}
pub fn take_kernel_rpc_reasons() -> u8 {
    smp::take_kernel_rpc()
}
pub const fn is_crash_stop_interrupt(_interrupt: hyper::hal::interrupt::InterruptId) -> bool {
    false
}
pub const fn broadcast_crash_stop() -> bool {
    false
}
pub fn validate_vsysreg() -> Result<(), guest::ValidationError> {
    guest::validate()
}

#[cfg(feature = "kernel-self-test")]
pub const fn guest_execution_available() -> bool {
    true
}
/// # Safety
/// Assembly entry must establish the selected machine boot environment.
pub(crate) unsafe fn prepare_boot(
    hart_id: usize,
    dtb_address: usize,
    boot_counter_ticks: u64,
) -> crate::hal::platform::ProtocolInputs {
    super::time::record_boot_counter(boot_counter_ticks);
    if !smp::initialize_boot_hart(hart_id as u64) {
        halt()
    }
    crate::hal::platform::ProtocolInputs::new(dtb_address, None, None)
}

pub(crate) fn describe_runtime(_emit: impl FnMut(core::fmt::Arguments<'_>)) {}

pub struct PreparedInterruptVirtualization;

pub fn prepare_interrupt_virtualization(
    _host_timer_interrupt: Option<hyper::hal::interrupt::HostInterruptBinding>,
) -> Result<PreparedInterruptVirtualization, InterruptVirtualizationError> {
    Ok(PreparedInterruptVirtualization)
}

pub fn commit_interrupt_virtualization(
    _prepared: PreparedInterruptVirtualization,
) -> Result<(), InterruptVirtualizationError> {
    Ok(())
}

pub const fn interrupt_virtualization_description() -> Option<(u8, u8, u8, u8)> {
    None
}

/// The firmware claim is checked for every enabled hart before SMP admission.
/// Local admission additionally probes Sv39, Sv39x4 and the Sstc control bit.
pub(crate) fn riscv_guest_baseline_available() -> bool {
    platform::guest_baseline_available()
}

pub(crate) use smp::prepare_secondary_entry;
