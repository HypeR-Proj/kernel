// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

mod address;
mod address_layout;
mod atomics;
mod barrier;
mod cache;
mod context;
mod exception;
mod gic_cpu_interface;
mod guest_cpu_contract;
mod guest_cpu_model;
mod host;
mod interrupt_controller;
mod interrupt_virtualization;
mod interrupts;
mod kaslr;
mod lower_el;
mod memory;
mod platform;
mod protection;
mod psci;
pub mod registers;
mod smp;
mod stage2;
mod stage2_retirement;
mod timer;
mod translation_identifiers;
mod user;
mod user_contract;
mod user_entry;
mod user_machine;
mod vgic;
mod vm_interrupt;
mod vm_timer;
mod vm_vcpu;
mod vsysreg;

pub use atomics::AtomicCapabilities;
pub(crate) use atomics::capabilities as atomic_capabilities;
pub use barrier::Aarch64Barrier as ArchitectureBarrier;
pub use cache::Aarch64Cache as ArchitectureCache;
pub use context::GuestRunError;
pub use context::{
    GuestAdministrativeStopReason, GuestRunExit, GuestSynchronousTerminal, GuestTerminalCause,
    GuestWaitReason, StoppedGuestRun,
};
pub use context::{ThreadContext, VcpuContext};
pub(crate) use context::{reset_stack_and_enter, run_on_emergency_stack, switch_thread_context};
pub use exception::CrashContext;
pub use exception::ValidationError as RuntimeVectorError;
pub(crate) use exception::{
    capture_crash_context, install_exception_stacks, install_local_runtime_vectors,
    install_runtime_vectors, validate_local_runtime_vectors, validate_runtime_vectors,
};
pub use gic_cpu_interface::Aarch64GicCpuInterface;
pub(crate) use gic_cpu_interface::{
    acknowledge_interrupt, broadcast_crash_stop, crash_stop_interrupt, current_gic_affinity,
    end_interrupt, is_crash_stop_interrupt, kernel_rpc_interrupt, notify_kernel_rpc,
    notify_reschedule, reschedule_interrupt,
};
pub fn take_kernel_rpc_reasons() -> u8 {
    smp::take_kernel_rpc()
}
pub const fn arm_kernel_rpc_source() {}
pub use host::mode_name as host_execution_mode_name;
pub use hyper::drivers::power::psci::Error as CpuPowerError;
pub use interrupt_controller::{
    Aarch64InterruptController as ArchitectureInterruptController,
    Error as InterruptControllerError,
};
pub(crate) use interrupt_virtualization::description as interrupt_virtualization_description;
pub use interrupt_virtualization::{
    Error as InterruptVirtualizationError, Prepared as PreparedInterruptVirtualization,
};
pub(crate) use interrupt_virtualization::{
    commit as commit_interrupt_virtualization, prepare as prepare_interrupt_virtualization,
};
pub use interrupts::LocalInterruptMask;
pub(crate) use interrupts::{
    disable_all as disable_all_interrupts, enable_irq as enable_local_irq,
    irq_enabled as local_irq_enabled, mask_irq as mask_local_irq,
};
pub use kaslr::Error as KaslrError;
pub(crate) use kaslr::select as select_kaslr_layout;
pub(crate) use memory::bootstrap_stack_bounds;
#[cfg(CONFIG_CRASH_CONSOLE)]
pub use memory::inspect_mapping as inspect_stage1_mapping;
pub use memory::{
    Aarch64AddressTranslation as ArchitectureAddressTranslation, ActivationContext,
    Error as MemoryError, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};
pub(crate) use platform::decode_platform_interrupt;
pub use platform::{
    Error as PlatformDiscoveryError, EssentialDeviceDiscovery, EssentialPlatformInfo,
};
pub(crate) use protection::{enable_local_memory_protection, local_memory_protection_enabled};
pub use psci::Aarch64Psci as ArchitectureCpuPower;
pub use smp::SecondaryBootParameters;
pub(crate) use smp::{
    current_cpu_index, current_hardware_id, secondary_entry_physical, send_event,
};
pub(crate) use stage2::retire_root_local as retire_guest_stage2_root_local;
pub use stage2::{Error as Stage2Error, Stage2AddressSpace};
pub(crate) use stage2::{
    publish_changes as publish_guest_stage2_changes,
    synchronize_local as synchronize_guest_stage2_local,
};
pub use stage2_retirement::Request as GuestStage2RetirementRequest;
pub use timer::{
    ArmGenericCounter as ArchitectureCounter, El2PhysicalTimer as ArchitectureTimer,
    Error as TimerError,
};
pub use user::UserMachineContractError;
pub(crate) use user::assert_kernel_pan;
pub(crate) use user::{
    application_address_limit, user_address_limit, user_translation_identifier_bits,
};
pub(crate) use user::{copy_from_exposed, copy_to_exposed};
pub use user_entry::Error as UserEntryError;
#[cfg(feature = "kernel-self-test")]
pub(crate) use user_entry::direct_native_call_count_for_test;
pub(crate) use user_entry::run_user;
pub(crate) use user_entry::{
    CompletionFailure as UserCompletionFailure, ReturnCapability as UserReturnCapability,
    UserContext, UserExit,
};
pub use user_machine::Error as UserAddressSpaceError;
pub(crate) use user_machine::{
    LocalActivation as UserLocalActivation, LocalIdentity as UserLocalIdentity,
    LocalOperation as UserLocalOperation, LocalRequest as UserLocalRequest,
    MappingPage as UserMappingPage, PreparedAddressSpace as PreparedUserAddressSpace,
};
pub(crate) use user_machine::{
    activate_local as activate_user_local, deactivate_local as deactivate_user_local,
    local_identity_is_active as user_local_identity_is_active,
    prepare_address_space as prepare_host_user_address_space,
    service_local_request as service_user_local_request,
};
pub use vgic::Error as VirtualInterruptError;
pub use vgic::{
    Capabilities as VgicCapabilities, CpuContext as VgicCpuContext, Error as VgicError,
};
pub(crate) use vgic::{
    disable as disable_vgic, maintenance_state as vgic_maintenance_state,
    validate_context_switch as validate_vgic,
};
pub use vm_interrupt::{Error as VmInterruptError, VmInterruptController};
pub use vm_vcpu::Error as VcpuInterruptError;
pub use vm_vcpu::GicAccessError;
pub use vm_vcpu::StoppedDeactivationFailure;
pub(crate) use vm_vcpu::{
    access_guest_gic, activate as activate_vcpu_hardware, deactivate as deactivate_vcpu_hardware,
    deactivate_stopped as deactivate_stopped_vcpu_hardware,
    handle_maintenance_interrupt as handle_virtualization_maintenance_interrupt,
    handle_virtual_timer_interrupt as handle_guest_virtual_timer_interrupt,
    inject_timer_for_validation,
    maintenance_interrupt_pending as virtualization_maintenance_pending,
    quiesce_virtual_interrupt_delivery, reconcile_active_interrupts, request_guest_exit,
    update_guest_device_interrupt, update_saved_guest_device_interrupt,
};
pub use vsysreg::ValidationError as GuestValidationError;
pub(crate) use vsysreg::validate as validate_vsysreg;
pub use vsysreg::{GuestSyncAction, GuestSyncExit, GuestSyncFailure};
pub(crate) use vsysreg::{
    apply_guest_sync_action, decode_guest_memory_fault, decode_guest_mmio_access,
    decode_guest_sync, handle_guest_sync,
};

#[cfg(feature = "kernel-self-test")]
pub fn guest_execution_available() -> bool {
    gic_cpu_interface::guest_interrupts_supported()
}

pub fn initialize_cpu_power(
    info: hyper::platform::CpuPowerInfo,
) -> Result<ArchitectureCpuPower, CpuPowerError> {
    match info {
        hyper::platform::CpuPowerInfo::Psci(info) => psci::bind(info),
        hyper::platform::CpuPowerInfo::Sbi(_) | hyper::platform::CpuPowerInfo::X86Apic(_) => {
            Err(CpuPowerError::NotSupported)
        }
    }
}

/// Checks that a secondary CPU supports the backend selected by the boot CPU.
pub const fn prepare_primary_cpu_admission() -> bool {
    true
}

pub fn secondary_cpu_is_compatible() -> bool {
    address::current_cpu_is_compatible()
        && atomics::current_cpu_supports_selected_backend()
        && cache::current_cpu_is_compatible()
        && guest_cpu_model::current_cpu_is_compatible()
        && host::current_cpu_is_compatible()
        && user::current_cpu_is_compatible()
}

pub fn register_secondary_hardware_id(cpu_index: usize, hardware_id: u64) -> bool {
    smp::register_cpu(cpu_index, hardware_id)
}

pub fn mark_current_cpu_online() {}

pub fn prepare_timekeeping(_platform: &EssentialPlatformInfo) -> Result<(), TimerError> {
    Ok(())
}

pub fn prepare_cache(
    _platform: &EssentialPlatformInfo,
) -> Result<(), hyper::hal::cache::CacheError> {
    cache::prepare_boot_cpu()
}

pub fn decode_kernel_timer(
    info: hyper::platform::TimerInfo,
) -> Result<crate::arch::time::Description, crate::arch::time::DescriptionError> {
    if info.kind != hyper::platform::TimerKind::ArmGeneric {
        return Err(crate::arch::time::DescriptionError::UnsupportedTimer);
    }
    if info.hypervisor_physical.trigger != hyper::platform::PlatformInterruptTrigger::Level
        || info.virtual_timer.trigger != hyper::platform::PlatformInterruptTrigger::Level
    {
        return Err(crate::arch::time::DescriptionError::InvalidInterruptTrigger);
    }
    Ok(crate::arch::time::Description {
        hardware: info.hypervisor_physical,
        guest_virtual_interrupt: hyper::hal::interrupt::InterruptId::new(
            info.virtual_timer.interrupt,
        ),
        map_guest_virtual_interrupt: true,
    })
}

pub use vm_vcpu::InitializationError as VirtualDeviceInitializationError;

pub fn initialize_virtual_devices(
    _timer_interrupt: hyper::hal::interrupt::InterruptId,
    host_timer_interrupt: Option<hyper::hal::interrupt::HostInterruptBinding>,
) -> Result<(), VirtualDeviceInitializationError> {
    host_timer_interrupt.ok_or(vm_vcpu::InitializationError::MissingHostTimerInterrupt)?;
    Ok(())
}

/// Applies the local interrupt state required immediately before guest entry.
///
/// `AArch64` must keep IRQs masked until `ERET`. Once `ELR_EL2` and `SPSR_EL2`
/// contain guest state, an IRQ taken by the entry trampoline would overwrite
/// those registers with its host return state. The exception tail could then
/// save that host state as the guest context and resume through a corrupted
/// entry transaction. Interrupts routed to EL2 remain able to preempt the
/// lower-EL guest after `ERET`.
pub const fn prepare_interrupts_for_guest_entry() {}

unsafe extern "C" {
    fn aarch64_activate_final_address_space(
        transition_root: u64,
        kernel_root: u64,
        virtual_base: u64,
        stack_top: u64,
        tcr_el2: u64,
    ) -> !;
}

/// Prepares the architecture-owned final address space.
///
/// # Safety
///
/// The bootstrap identity map must cover every page the early allocator may
/// return.
pub unsafe fn prepare_address_space(
    allocator: &mut hyper::mm::BootAllocator,
    platform: &hyper::platform::PlatformInfo,
    image: hyper::hal::memory::KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, MemoryError> {
    // SAFETY: This function forwards the bootstrap identity-map and allocator
    // validity contract directly to the architecture memory builder.
    unsafe { memory::prepare(allocator, platform, image, kernel_base) }
}

/// Activates final stage-1 translation and enters the high kernel alias.
///
/// # Safety
///
/// `memory` must remain installed in global boot state for the lifetime of the
/// active address space.
pub unsafe fn activate_memory(context: ActivationContext) -> ! {
    // SAFETY: The caller guarantees that the context names the globally pinned
    // final hierarchy, high alias, and mapped stack consumed by this transition.
    unsafe {
        aarch64_activate_final_address_space(
            context.transition_root.get(),
            context.kernel_root.get(),
            context.kernel_base,
            context.stack_top.get(),
            context.tcr_el2,
        )
    }
}

/// Keeps this processing element in a side-effect-free idle loop.
pub fn halt() -> ! {
    loop {
        wait_for_interrupt();
    }
}

/// Suspends this processing element until an interrupt or event is observed.
pub fn wait_for_interrupt() {
    // SAFETY: WFI only suspends this processing element until an event or
    // interrupt occurs and does not access memory.
    unsafe { core::arch::asm!("wfi", options(nostack, preserves_flags)) };
}

/// Waits for an interrupt after scheduler work was checked with IRQs masked.
///
/// IRQ delivery is opened around WFI because an EL2 physical timer PPI is not
/// required to retire a masked WFI on every implementation. If an interrupt
/// publishes runnable work in the enable-to-WFI window, IRQ-tail preemption
/// switches away from the idle Thread; this continuation reaches WFI only
/// after idle is runnable again. The caller's outer guard still owns and
/// restores the exact mask state after this function returns.
pub fn wait_for_interrupt_masked() {
    // SAFETY: DAIF.I is masked on entry. These instructions temporarily admit
    // IRQ handlers, wait at EL2, then reestablish the caller-owned mask before
    // returning. Exception entry preserves the remaining DAIF fields.
    unsafe {
        core::arch::asm!(
            "msr daifclr, #2",
            "wfi",
            "msr daifset, #2",
            options(nostack, preserves_flags)
        );
    }
}

pub const fn port_io() -> Option<hyper::hal::io::PortIo> {
    None
}

pub const fn service_stage1_tlb_shootdown() -> bool {
    true
}

/// Rust continuation used while the architecture bootstrap is still active.
///
/// This is not the kernel entry: the permanent address space and stack are
/// prepared from here, then assembly completes the transition to
/// `start_kernel`.
/// # Safety
/// Assembly entry must establish the selected machine boot environment.
pub(crate) unsafe fn prepare_boot(
    dtb_address: usize,
    _unused: usize,
    boot_counter_ticks: u64,
) -> crate::hal::platform::ProtocolInputs {
    super::time::record_boot_counter(boot_counter_ticks);
    address::initialize().unwrap_or_else(|_| halt());
    atomics::initialize();
    if host::initialize().is_err() {
        halt()
    }
    if !guest_cpu_model::initialize_boot_cpu() {
        halt()
    }
    if !smp::initialize_boot_cpu() {
        halt()
    }
    crate::hal::platform::ProtocolInputs::new(dtb_address, None, None)
}

pub(crate) fn describe_runtime(mut emit: impl FnMut(core::fmt::Arguments<'_>)) {
    let address = address::capabilities();
    emit(format_args!(
        "HypeR: AArch64 host execution mode: {}",
        host_execution_mode_name()
    ));
    emit(format_args!(
        "HypeR: translation identifiers: {}-bit ASID, {}-bit VMID",
        address.asid_bits, address.vmid_bits
    ));
    emit(format_args!(
        "HypeR: AArch64 execution protection: PXN/UXN, WXN=on"
    ));
    emit(format_args!(
        "HypeR: AArch64 address space: {}-bit VA/{} levels, {}-bit PA (CPU {}-bit), {}-bit IPA/{} levels",
        address.virtual_address_bits,
        address.stage1_levels(),
        address.physical_address_bits,
        address.supported_physical_address_bits,
        address.intermediate_physical_address_bits,
        address.stage2_levels(),
    ));
}

pub use gic_cpu_interface::guest_interrupts_supported;

mod bootstrap_map;

/// # Safety
/// Runs once with MMU off, after relocation/BSS/stack setup. Firmware supplies
/// a stable readable DTB; table points to the exclusively owned L1 boot page.
#[unsafe(no_mangle)]
unsafe extern "C" fn aarch64_prepare_boot_map(dtb: usize, table: *mut [u64; 512]) -> u64 {
    // SAFETY: The boot ABI guarantees the complete firmware DTB is readable.
    let Ok(platform) = (unsafe { hyper::platform::fdt::discover(dtb) }) else {
        return 0;
    };
    // SAFETY: Assembly passes the aligned, exclusive bootstrap L1 page.
    u64::from(bootstrap_map::populate(unsafe { &mut *table }, &platform))
}

/// Guest interrupt-controller revision of the selected reference board.
pub(crate) fn guest_gic_version() -> u32 {
    if vgic::v2::guest_physical().is_some() {
        2
    } else {
        3
    }
}

pub(crate) const fn maximum_guest_vcpus() -> u32 {
    hyper::vm::arm::psci::MAX_CPUS as u32
}

pub(crate) use smp::prepare_secondary_entry;

pub(crate) fn guest_translation_identifier_bits() -> u8 {
    address::capabilities().vmid_bits
}

pub(crate) use stage2::invalidate_namespace_local as invalidate_guest_translation_namespace_local;
