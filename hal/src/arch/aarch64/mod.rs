// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod irq;

mod vm;

mod atomics;
mod barrier;
mod cache;
mod context;
mod exception;
mod fp;
mod host;
mod lower_el;
mod memory;
mod platform;
#[cfg(feature = "pmu-profiling")]
pub(crate) mod pmu;
mod psci;
mod random;
pub(crate) use random::seed as random_seed;
pub mod registers;
mod smp;
mod timer;
mod user;

pub use crate::arch::aarch64::irq::gic::Aarch64GicCpuInterface;
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
pub(crate) use exception::InterruptFrame;
pub use exception::ValidationError as RuntimeVectorError;
pub(crate) use exception::{
    capture_crash_context, install_exception_stacks, install_local_runtime_vectors,
    install_runtime_vectors, validate_local_runtime_vectors, validate_runtime_vectors,
};
pub(crate) use {
    crate::arch::aarch64::irq::gic::acknowledge_interrupt,
    crate::arch::aarch64::irq::gic::broadcast_crash_stop,
    crate::arch::aarch64::irq::gic::crash_stop_interrupt,
    crate::arch::aarch64::irq::gic::current_gic_affinity,
    crate::arch::aarch64::irq::gic::end_interrupt,
    crate::arch::aarch64::irq::gic::is_crash_stop_interrupt,
    crate::arch::aarch64::irq::gic::kernel_rpc_interrupt,
    crate::arch::aarch64::irq::gic::notify_kernel_rpc,
    crate::arch::aarch64::irq::gic::notify_reschedule,
    crate::arch::aarch64::irq::gic::reschedule_interrupt,
};
pub fn take_kernel_rpc_reasons() -> u8 {
    smp::take_kernel_rpc()
}
pub const fn arm_kernel_rpc_source() {}
pub use crate::arch::aarch64::irq::LocalInterruptMask;
pub use crate::arch::aarch64::memory::kaslr::Error as KaslrError;
pub(crate) use crate::arch::aarch64::memory::kaslr::select as select_kaslr_layout;
pub use crate::arch::aarch64::user::entry::Error as UserEntryError;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use crate::arch::aarch64::user::entry::direct_native_call_count_for_test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use crate::arch::aarch64::user::entry::native_run_counts_for_test;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use crate::arch::aarch64::user::entry::syscall_fp_resident_for_test;
pub use crate::arch::aarch64::user::memory::Error as UserAddressSpaceError;
pub(crate) use crate::arch::aarch64::vm::interrupt_virtualization::description as interrupt_virtualization_description;
pub(crate) use crate::arch::aarch64::vm::stage2::retire_root_local as retire_guest_stage2_root_local;
pub use crate::arch::aarch64::vm::stage2::retirement::Request as GuestStage2RetirementRequest;
pub use crate::arch::aarch64::vm::sysreg::ValidationError as GuestValidationError;
pub(crate) use crate::arch::aarch64::vm::sysreg::validate as validate_vsysreg;
pub use crate::arch::aarch64::vm::vcpu::Error as VcpuInterruptError;
pub use crate::arch::aarch64::vm::vcpu::GicAccessError;
pub use crate::arch::aarch64::vm::vcpu::StoppedDeactivationFailure;
pub use crate::arch::aarch64::vm::vgic::Error as VirtualInterruptError;
pub use host::mode_name as host_execution_mode_name;
pub use hyper_drivers::power::psci::Error as CpuPowerError;
pub(crate) use memory::bootstrap_stack_bounds;
#[cfg(feature = "crash-console")]
pub use memory::inspect_mapping as inspect_stage1_mapping;
pub use memory::{
    Aarch64AddressTranslation as ArchitectureAddressTranslation, ActivationContext,
    Error as MemoryError, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};
pub(crate) use platform::decode_platform_interrupt;
pub use platform::{
    Error as PlatformDiscoveryError, EssentialDeviceDiscovery, EssentialPlatformInfo,
};
pub use psci::Aarch64Psci as ArchitectureCpuPower;
pub use smp::SecondaryBootParameters;
pub(crate) use smp::{
    current_cpu_index, current_hardware_id, secondary_entry_physical, send_event,
};
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
pub use {
    crate::arch::aarch64::irq::controller::Aarch64InterruptController as ArchitectureInterruptController,
    crate::arch::aarch64::irq::controller::Error as InterruptControllerError,
};
pub(crate) use {
    crate::arch::aarch64::irq::disable_all as disable_all_interrupts,
    crate::arch::aarch64::irq::enable_irq as enable_local_irq,
    crate::arch::aarch64::irq::irq_enabled as local_irq_enabled,
    crate::arch::aarch64::irq::mask_irq as mask_local_irq,
};
pub(crate) use {
    crate::arch::aarch64::memory::protection::enable_local_memory_protection,
    crate::arch::aarch64::memory::protection::local_memory_protection_enabled,
};
pub(crate) use {
    crate::arch::aarch64::user::entry::CompletionFailure as UserCompletionFailure,
    crate::arch::aarch64::user::entry::ReturnCapability as UserReturnCapability,
    crate::arch::aarch64::user::entry::UserContext, crate::arch::aarch64::user::entry::UserExit,
};
pub(crate) use {
    crate::arch::aarch64::user::entry::run_user,
    crate::arch::aarch64::user::entry::suspend_current_user,
};
pub(crate) use {
    crate::arch::aarch64::user::memory::LocalActivation as UserLocalActivation,
    crate::arch::aarch64::user::memory::LocalIdentity as UserLocalIdentity,
    crate::arch::aarch64::user::memory::LocalOperation as UserLocalOperation,
    crate::arch::aarch64::user::memory::LocalRequest as UserLocalRequest,
    crate::arch::aarch64::user::memory::PreparedAddressSpace as PreparedUserAddressSpace,
};
pub(crate) use {
    crate::arch::aarch64::user::memory::activate_local as activate_user_local,
    crate::arch::aarch64::user::memory::deactivate_local as deactivate_user_local,
    crate::arch::aarch64::user::memory::edit_address_space as edit_host_user_address_space,
    crate::arch::aarch64::user::memory::local_identity_is_active as user_local_identity_is_active,
    crate::arch::aarch64::user::memory::prepare_address_space as prepare_host_user_address_space,
    crate::arch::aarch64::user::memory::service_local_request as service_user_local_request,
};
pub use {
    crate::arch::aarch64::vm::interrupt::Error as VmInterruptError,
    crate::arch::aarch64::vm::interrupt::VmInterruptController,
};
pub use {
    crate::arch::aarch64::vm::interrupt_virtualization::Error as InterruptVirtualizationError,
    crate::arch::aarch64::vm::interrupt_virtualization::Prepared as PreparedInterruptVirtualization,
};
pub(crate) use {
    crate::arch::aarch64::vm::interrupt_virtualization::commit as commit_interrupt_virtualization,
    crate::arch::aarch64::vm::interrupt_virtualization::prepare as prepare_interrupt_virtualization,
};
pub use {
    crate::arch::aarch64::vm::stage2::Error as Stage2Error,
    crate::arch::aarch64::vm::stage2::Stage2AddressSpace,
};
pub(crate) use {
    crate::arch::aarch64::vm::stage2::publish_changes as publish_guest_stage2_changes,
    crate::arch::aarch64::vm::stage2::synchronize_local as synchronize_guest_stage2_local,
};
pub use {
    crate::arch::aarch64::vm::sysreg::GuestSyncAction,
    crate::arch::aarch64::vm::sysreg::GuestSyncExit,
    crate::arch::aarch64::vm::sysreg::GuestSyncFailure,
};
pub(crate) use {
    crate::arch::aarch64::vm::sysreg::apply_guest_sync_action,
    crate::arch::aarch64::vm::sysreg::decode_guest_memory_fault,
    crate::arch::aarch64::vm::sysreg::decode_guest_mmio_access,
    crate::arch::aarch64::vm::sysreg::decode_guest_sync,
    crate::arch::aarch64::vm::sysreg::handle_guest_sync,
};
pub(crate) use {
    crate::arch::aarch64::vm::vcpu::access_guest_gic,
    crate::arch::aarch64::vm::vcpu::activate as activate_vcpu_hardware,
    crate::arch::aarch64::vm::vcpu::deactivate as deactivate_vcpu_hardware,
    crate::arch::aarch64::vm::vcpu::deactivate_stopped as deactivate_stopped_vcpu_hardware,
    crate::arch::aarch64::vm::vcpu::handle_maintenance_interrupt as handle_virtualization_maintenance_interrupt,
    crate::arch::aarch64::vm::vcpu::handle_virtual_timer_interrupt as handle_guest_virtual_timer_interrupt,
    crate::arch::aarch64::vm::vcpu::inject_saved_guest_device_interrupt,
    crate::arch::aarch64::vm::vcpu::inject_timer_for_validation,
    crate::arch::aarch64::vm::vcpu::maintenance_interrupt_pending as virtualization_maintenance_pending,
    crate::arch::aarch64::vm::vcpu::quiesce_virtual_interrupt_delivery,
    crate::arch::aarch64::vm::vcpu::request_guest_exit,
    crate::arch::aarch64::vm::vcpu::update_guest_device_interrupt,
    crate::arch::aarch64::vm::vcpu::update_saved_guest_device_interrupt,
    crate::arch::aarch64::vm::vcpu::virtual_timer_eoi_pending,
};
pub use {
    crate::arch::aarch64::vm::vgic::Capabilities as VgicCapabilities,
    crate::arch::aarch64::vm::vgic::CpuContext as VgicCpuContext,
    crate::arch::aarch64::vm::vgic::Error as VgicError,
};
pub(crate) use {
    crate::arch::aarch64::vm::vgic::disable as disable_vgic,
    crate::arch::aarch64::vm::vgic::maintenance_state as vgic_maintenance_state,
    crate::arch::aarch64::vm::vgic::validate_context_switch as validate_vgic,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub fn guest_execution_available() -> bool {
    crate::arch::aarch64::irq::gic::guest_interrupts_supported()
}

pub fn initialize_cpu_power(
    info: hyper_platform::CpuPowerInfo,
) -> Result<ArchitectureCpuPower, CpuPowerError> {
    match info {
        hyper_platform::CpuPowerInfo::Psci(info) => psci::bind(info),
        hyper_platform::CpuPowerInfo::Sbi(_) | hyper_platform::CpuPowerInfo::X86Apic(_) => {
            Err(CpuPowerError::NotSupported)
        }
    }
}

/// Checks that a secondary CPU supports the backend selected by the boot CPU.
pub const fn prepare_primary_cpu_admission() -> bool {
    true
}

pub fn secondary_cpu_is_compatible() -> bool {
    crate::arch::aarch64::memory::address::current_cpu_is_compatible()
        && atomics::current_cpu_supports_selected_backend()
        && cache::current_cpu_is_compatible()
        && crate::arch::aarch64::vm::cpu_model::current_cpu_is_compatible()
        && host::current_cpu_is_compatible()
        && user::current_cpu_is_compatible()
}

pub fn register_secondary_hardware_id(cpu_index: usize, hardware_id: u64) -> bool {
    smp::register_cpu(cpu_index, hardware_id)
}

pub fn mark_current_cpu_online() {
    // SAFETY: Secondary compatibility admission validated FP/SIMD support;
    // this CPU has not run lower-world code and still masks local exceptions.
    unsafe { fp::initialize_cpu() };
}

pub fn prepare_timekeeping(_platform: &EssentialPlatformInfo) -> Result<(), TimerError> {
    Ok(())
}

pub fn prepare_cache(
    _platform: &EssentialPlatformInfo,
) -> Result<(), hyper::hal::cache::CacheError> {
    cache::prepare_boot_cpu()
}

pub fn decode_kernel_timer(
    info: hyper_platform::TimerInfo,
) -> Result<crate::arch::time::Description, crate::arch::time::DescriptionError> {
    if info.kind != hyper_platform::TimerKind::ArmGeneric {
        return Err(crate::arch::time::DescriptionError::UnsupportedTimer);
    }
    if info.hypervisor_physical.trigger != hyper_platform::PlatformInterruptTrigger::Level
        || info.virtual_timer.trigger != hyper_platform::PlatformInterruptTrigger::Level
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

pub use crate::arch::aarch64::vm::vcpu::InitializationError as VirtualDeviceInitializationError;

pub fn initialize_virtual_devices(
    _timer_interrupt: hyper::hal::interrupt::InterruptId,
    host_timer_interrupt: Option<hyper::hal::interrupt::HostInterruptBinding>,
) -> Result<(), VirtualDeviceInitializationError> {
    host_timer_interrupt
        .ok_or(crate::arch::aarch64::vm::vcpu::InitializationError::MissingHostTimerInterrupt)?;
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
    platform: &hyper_platform::PlatformInfo,
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
    crate::arch::aarch64::memory::address::initialize().unwrap_or_else(|_| halt());
    atomics::initialize();
    if host::initialize().is_err() {
        halt()
    }
    if !crate::arch::aarch64::vm::cpu_model::initialize_boot_cpu() {
        halt()
    }
    // SAFETY: The frozen model requires FP/SIMD. Boot still masks exceptions
    // and has not admitted any lower-world owner on this CPU.
    unsafe { fp::initialize_cpu() };
    if !smp::initialize_boot_cpu() {
        halt()
    }
    crate::hal::platform::ProtocolInputs::new(dtb_address, None, None)
}

pub(crate) fn describe_runtime(mut emit: impl FnMut(core::fmt::Arguments<'_>)) {
    let address = crate::arch::aarch64::memory::address::capabilities();
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

pub use crate::arch::aarch64::irq::gic::guest_interrupts_supported;

/// # Safety
/// Runs once with MMU off, after relocation/BSS/stack setup. Firmware supplies
/// a stable readable DTB; table points to the exclusively owned L1 boot page.
#[unsafe(no_mangle)]
unsafe extern "C" fn aarch64_prepare_boot_map(dtb: usize, table: *mut [u64; 512]) -> u64 {
    // SAFETY: The boot ABI guarantees the complete firmware DTB is readable.
    let Ok(platform) = (unsafe { hyper_platform::fdt::discover(dtb) }) else {
        return 0;
    };
    // SAFETY: Assembly passes the aligned, exclusive bootstrap L1 page.
    u64::from(crate::arch::aarch64::memory::bootstrap_map::populate(
        unsafe { &mut *table },
        &platform,
    ))
}

/// Guest interrupt-controller revision of the selected reference board.
pub(crate) fn guest_gic_version() -> u32 {
    if crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some() {
        2
    } else {
        3
    }
}

pub(crate) const fn maximum_guest_vcpus() -> u32 {
    hyper_vm_models::arm::psci::MAX_CPUS as u32
}

pub(crate) use smp::prepare_secondary_entry;

pub(crate) fn guest_translation_identifier_bits() -> u8 {
    crate::arch::aarch64::memory::address::capabilities().vmid_bits
}

pub(crate) use crate::arch::aarch64::vm::stage2::invalidate_namespace_local as invalidate_guest_translation_namespace_local;

#[cfg(all(
    target_arch = "aarch64",
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub(crate) use fp::fp_state_counts_for_test;
