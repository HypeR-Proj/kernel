// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

pub(crate) mod irq;
pub(crate) mod vm;

mod barrier;
mod boot_protocol;
mod cache;
mod context;
mod cpu_power;
mod exception;
mod features;
mod memory;
mod platform;
#[allow(dead_code)]
pub mod registers;
mod smp;
mod timer;

use core::arch::asm;

pub type InterruptVirtualizationError = core::convert::Infallible;

pub use crate::arch::x86_64::irq::LocalInterruptMask;
pub use crate::arch::x86_64::memory::kaslr::Error as KaslrError;
pub(crate) use crate::arch::x86_64::memory::kaslr::select as select_kaslr_layout;
pub use crate::arch::x86_64::vm::admission::ValidationError as GuestValidationError;
pub use crate::arch::x86_64::vm::vcpu::Error as VcpuInterruptError;
pub use barrier::X86_64Barrier as ArchitectureBarrier;
pub use cache::X86_64Cache as ArchitectureCache;
pub use context::{ThreadContext, VcpuContext, VirtualInterruptError};
pub(crate) use context::{reset_stack_and_enter, switch_thread_context};
pub use cpu_power::{Error as CpuPowerError, X2ApicCpuPower as ArchitectureCpuPower};
pub use exception::CrashContext;
pub use exception::ValidationError as RuntimeVectorError;
pub(crate) use exception::{
    bootstrap_stack_bounds, capture_crash_context, install_exception_stacks,
    install_local_runtime_vectors, install_runtime_vectors, run_on_emergency_stack,
    validate_local_runtime_vectors, validate_runtime_vectors,
};
#[cfg(feature = "crash-console")]
pub use memory::inspect_mapping as inspect_stage1_mapping;
pub use memory::{
    ActivationContext, Error as MemoryError, PreparedAddressSpace, SecondaryActivationContext,
    StackMapping, X86_64AddressTranslation as ArchitectureAddressTranslation,
};
pub(crate) use platform::decode_platform_interrupt;
pub use platform::{
    Error as PlatformDiscoveryError, EssentialDeviceDiscovery, EssentialPlatformInfo,
};
pub use smp::SecondaryBootParameters;
pub(crate) use smp::{
    current_cpu_index, current_hardware_id, notify_reschedule, secondary_entry_physical, send_event,
};
pub use timer::{
    Error as TimerError, TscCounter as ArchitectureCounter, TscDeadlineTimer as ArchitectureTimer,
};
pub use {
    crate::arch::x86_64::irq::controller::Error as InterruptControllerError,
    crate::arch::x86_64::irq::controller::X2ApicController as ArchitectureInterruptController,
};
pub(crate) use {
    crate::arch::x86_64::irq::disable_all as disable_all_interrupts,
    crate::arch::x86_64::irq::enable_irq as enable_local_irq,
    crate::arch::x86_64::irq::irq_enabled as local_irq_enabled,
    crate::arch::x86_64::irq::mask_irq as mask_local_irq,
};
pub use {
    crate::arch::x86_64::vm::interrupt::Error as VmInterruptError,
    crate::arch::x86_64::vm::interrupt::VmInterruptController,
};
pub use {
    crate::arch::x86_64::vm::stage2::Error as Stage2Error,
    crate::arch::x86_64::vm::stage2::Stage2AddressSpace,
};
pub(crate) use {
    crate::arch::x86_64::vm::vcpu::activate as activate_vcpu_hardware,
    crate::arch::x86_64::vm::vcpu::deactivate as deactivate_vcpu_hardware,
    crate::arch::x86_64::vm::vcpu::handle_maintenance_interrupt as handle_virtualization_maintenance_interrupt,
    crate::arch::x86_64::vm::vcpu::handle_virtual_timer_interrupt as handle_guest_virtual_timer_interrupt,
    crate::arch::x86_64::vm::vcpu::maintenance_interrupt_pending as virtualization_maintenance_pending,
    crate::arch::x86_64::vm::vcpu::quiesce_virtual_interrupt_delivery,
};

pub const fn interrupt_virtualization_description() -> Option<(u8, u8, u8, u8)> {
    None
}

pub type VirtualDeviceInitializationError = core::convert::Infallible;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicCapabilities;

impl AtomicCapabilities {
    pub const fn backend_name(self) -> &'static str {
        "x86-64 locked instructions"
    }
}

pub const fn atomic_capabilities() -> AtomicCapabilities {
    AtomicCapabilities
}

pub fn service_stage1_tlb_shootdown() -> bool {
    crate::arch::x86_64::memory::tlb::service_pending();
    true
}

/// x86-64 enables NXE before long mode and final page tables contain no
/// writable executable leaves, so no additional per-CPU transition is needed.
pub fn enable_local_memory_protection() {}

pub const fn local_memory_protection_enabled() -> bool {
    true
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn stage1_shootdown_count_for_test() -> u64 {
    crate::arch::x86_64::memory::tlb::completed_count_for_test()
}

pub fn initialize_cpu_power(
    info: hyper_platform::CpuPowerInfo,
) -> Result<ArchitectureCpuPower, CpuPowerError> {
    cpu_power::bind(info)
}

pub const fn prepare_primary_cpu_admission() -> bool {
    true
}

pub fn secondary_cpu_is_compatible() -> bool {
    true
}

pub fn register_secondary_hardware_id(cpu_index: usize, hardware_id: u64) -> bool {
    smp::register_cpu(cpu_index, hardware_id)
}

pub fn mark_current_cpu_online() {
    smp::mark_current_cpu_online();
}

pub fn prepare_timekeeping(platform: &EssentialPlatformInfo) -> Result<(), TimerError> {
    timer::set_frequency(platform.tsc_frequency)
}

pub fn prepare_cache(
    _platform: &EssentialPlatformInfo,
) -> Result<(), hyper::hal::cache::CacheError> {
    cache::initialize()
}

pub fn decode_kernel_timer(
    info: hyper_platform::TimerInfo,
) -> Result<crate::arch::time::Description, crate::arch::time::DescriptionError> {
    if info.kind != hyper_platform::TimerKind::X86TscDeadline
        || info.hypervisor_physical.trigger != hyper_platform::PlatformInterruptTrigger::Edge
    {
        return Err(crate::arch::time::DescriptionError::UnsupportedTimer);
    }
    Ok(crate::arch::time::Description {
        hardware: info.hypervisor_physical,
        guest_virtual_interrupt: hyper::hal::interrupt::InterruptId::new(
            info.virtual_timer.interrupt,
        ),
        map_guest_virtual_interrupt: false,
    })
}

pub fn initialize_virtual_devices(
    _timer_interrupt: hyper::hal::interrupt::InterruptId,
    _host_timer_interrupt: Option<hyper::hal::interrupt::HostInterruptBinding>,
) -> Result<(), VirtualDeviceInitializationError> {
    Ok(())
}

pub const fn prepare_interrupts_for_guest_entry() {}

/// Builds the permanent host address space while using bootstrap mappings.
///
/// # Safety
///
/// Allocator results must be directly writable, uniquely owned physical RAM.
pub unsafe fn prepare_address_space(
    allocator: &mut hyper::mm::BootAllocator,
    platform: &hyper_platform::PlatformInfo,
    image: hyper::hal::memory::KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, MemoryError> {
    // SAFETY: This function forwards its directly writable allocator contract.
    unsafe { memory::prepare(allocator, platform, image, kernel_base) }
}

unsafe extern "C" {
    fn x86_64_activate_final_address_space(root: u64, kernel_base: u64, stack_top: u64) -> !;
}

/// Activates a prepared address space and switches to its permanent stack.
///
/// # Safety
///
/// `context` must come from the retained prepared address space, with no live
/// reference depending on mappings removed by the transition.
pub unsafe fn activate_memory(context: ActivationContext) -> ! {
    // SAFETY: The caller guarantees this activation context remains backed and live.
    unsafe {
        x86_64_activate_final_address_space(
            context.root.get(),
            context.kernel_base,
            context.stack_top.get(),
        )
    }
}

pub fn halt() -> ! {
    loop {
        // SAFETY: CLI/HLT is valid at CPL0 and remains a compiler memory boundary.
        unsafe { asm!("cli", "hlt", options(nostack)) };
    }
}

/// Atomically enables maskable interrupts for HLT and returns with IF clear.
///
/// The scheduler checks its run queue with IF clear. STI's interrupt shadow
/// extends through the following HLT, so an interrupt cannot be handled in the
/// check-to-sleep window and then leave this CPU asleep with runnable work.
pub fn wait_for_interrupt_masked() {
    // SAFETY: STI, HLT, and CLI are valid at CPL0. The caller enters with IF
    // clear and restores the exact prior state after this function returns.
    unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
}

pub const fn port_io() -> Option<hyper::hal::io::PortIo> {
    // SAFETY: The callbacks implement byte port I/O and uphold PortIo's contract.
    Some(unsafe { hyper::hal::io::PortIo::new(read_port, write_port) })
}

unsafe fn read_port(port: u16) -> u8 {
    let value: u8;
    // SAFETY: The caller is authorized to access `port`; IN is valid at CPL0.
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nostack)) };
    value
}

unsafe fn write_port(port: u16, value: u8) {
    // SAFETY: The caller is authorized to access `port`; OUT is valid at CPL0.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nostack)) };
}

pub const fn crash_stop_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    None
}
pub const fn reschedule_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    Some(hyper::hal::interrupt::InterruptId::new(
        platform::RESCHEDULE_VECTOR,
    ))
}

pub const fn kernel_rpc_interrupt() -> Option<hyper::hal::interrupt::InterruptId> {
    Some(hyper::hal::interrupt::InterruptId::new(
        platform::KERNEL_RPC_VECTOR,
    ))
}

pub const fn arm_kernel_rpc_source() {}

pub fn notify_kernel_rpc(cpu: hyper::cpu::CpuIndex, reasons: u8) -> bool {
    smp::notify_kernel_rpc(cpu, reasons)
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

pub fn validate_vsysreg() -> Result<(), crate::arch::x86_64::vm::admission::ValidationError> {
    crate::arch::x86_64::vm::admission::validate()
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub fn guest_execution_available() -> bool {
    crate::arch::x86_64::vm::validate().is_ok()
}

/// # Safety
/// Assembly entry must establish the selected machine boot environment.
pub(crate) unsafe fn prepare_boot(
    boot_params: usize,
    _unused: usize,
    boot_counter_ticks: u64,
) -> crate::hal::platform::ProtocolInputs {
    super::time::record_boot_counter(boot_counter_ticks);
    if !smp::initialize_boot_cpu() {
        halt()
    }
    // SAFETY: The Linux boot protocol supplies a retained complete boot-params record.
    let inputs = match unsafe { boot_protocol::parse(boot_params) } {
        Ok(inputs) => inputs,
        Err(_) => halt(),
    };
    crate::hal::platform::ProtocolInputs::new(
        inputs.dtb_address,
        inputs.command_line,
        inputs.initial_ramdisk,
    )
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

pub(crate) use smp::prepare_secondary_entry;
