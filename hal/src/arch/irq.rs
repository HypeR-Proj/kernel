// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture host interrupt mechanisms.
//!
//! Kernel IRQ policy owns domains, handler lifetimes, routing, and failure
//! policy. This facade exposes only local masking, controller construction,
//! platform interrupt decoding, and scheduler cross-call notification. Guest
//! interrupt virtualization belongs to `arch::vm`.

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::{EntryAction, InterruptId, InterruptOrigin};
use hyper::sync::PublishedOnce;

#[derive(Clone, Copy)]
struct KernelRpcServices {
    poll: fn(),
    interrupt: fn(InterruptOrigin) -> EntryAction,
}

static KERNEL_RPC_SERVICES: PublishedOnce<KernelRpcServices> = PublishedOnce::new();

/// Immutable kernel policy callbacks reachable from physical-interrupt entry.
#[derive(Clone, Copy)]
pub(crate) struct InterruptEntryServices {
    dispatch: fn(InterruptId, InterruptOrigin) -> EntryAction,
    #[cfg(CONFIG_ARCH_RISCV64)]
    claim_external: fn(InterruptOrigin) -> Option<EntryAction>,
    stop: fn(super::exception::CrashContext) -> !,
}

impl InterruptEntryServices {
    pub(crate) const fn new(
        dispatch: fn(InterruptId, InterruptOrigin) -> EntryAction,
        #[cfg(CONFIG_ARCH_RISCV64)] claim_external: fn(InterruptOrigin) -> Option<EntryAction>,
        stop: fn(super::exception::CrashContext) -> !,
    ) -> Self {
        Self {
            dispatch,
            #[cfg(CONFIG_ARCH_RISCV64)]
            claim_external,
            stop,
        }
    }
}

static INTERRUPT_ENTRY_SERVICES: PublishedOnce<InterruptEntryServices> = PublishedOnce::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptEntryServiceError {
    AlreadyInstalled,
}

#[derive(Clone, Copy)]
pub(crate) struct InterruptEntryReady {
    _private: (),
}

pub(crate) fn install_interrupt_entry_services(
    services: InterruptEntryServices,
) -> Result<InterruptEntryReady, InterruptEntryServiceError> {
    INTERRUPT_ENTRY_SERVICES
        .publish(services)
        .map_err(|_| InterruptEntryServiceError::AlreadyInstalled)?;
    Ok(InterruptEntryReady { _private: () })
}

fn interrupt_entry_services() -> InterruptEntryServices {
    let Some(services) = INTERRUPT_ENTRY_SERVICES.get().copied() else {
        hyper::debug::invariant_failure("irq::interrupt_entry_services invariant")
    };
    services
}

pub(crate) fn dispatch_entry(interrupt: InterruptId, origin: InterruptOrigin) -> EntryAction {
    (interrupt_entry_services().dispatch)(interrupt, origin)
}

#[cfg(CONFIG_ARCH_RISCV64)]
pub(crate) fn claim_and_dispatch_external_entry(origin: InterruptOrigin) -> Option<EntryAction> {
    (interrupt_entry_services().claim_external)(origin)
}

pub(crate) fn stop_entry(context: super::exception::CrashContext) -> ! {
    (interrupt_entry_services().stop)(context)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelRpcServiceError {
    AlreadyInstalled,
}

pub use super::imp::{
    ArchitectureInterruptController as Controller, InterruptControllerError as ControllerError,
    LocalInterruptMask as LocalMask,
};

pub(crate) use super::imp::{
    decode_platform_interrupt as decode_platform, disable_all_interrupts as disable_all_sources,
    enable_local_irq as enable_local, local_irq_enabled as local_enabled,
    mask_local_irq as mask_local,
};

/// Returns the architecture-reserved physical reschedule interrupt, if any.
pub(crate) fn reschedule_interrupt() -> Option<InterruptId> {
    super::imp::reschedule_interrupt()
}

/// Prompts `cpu` to evaluate its already-published reschedule request.
pub(crate) fn notify_reschedule(cpu: CpuIndex) -> bool {
    super::imp::notify_reschedule(cpu)
}

pub(crate) fn kernel_rpc_interrupt() -> Option<InterruptId> {
    super::imp::kernel_rpc_interrupt()
}

pub(crate) fn arm_kernel_rpc_source() {
    super::imp::arm_kernel_rpc_source();
}

pub(crate) fn notify_kernel_rpc(cpu: CpuIndex, reasons: u8) -> bool {
    super::imp::notify_kernel_rpc(cpu, reasons)
}

pub(crate) fn take_kernel_rpc_reasons() -> u8 {
    super::imp::take_kernel_rpc_reasons()
}

/// Installs the allocation-free kernel dispatcher before its doorbell is armed.
pub(crate) fn install_kernel_rpc_services(
    poll: fn(),
    interrupt: fn(InterruptOrigin) -> EntryAction,
) -> Result<(), KernelRpcServiceError> {
    KERNEL_RPC_SERVICES
        .publish(KernelRpcServices { poll, interrupt })
        .map_err(|_| KernelRpcServiceError::AlreadyInstalled)
}

fn kernel_rpc_services() -> KernelRpcServices {
    let Some(services) = KERNEL_RPC_SERVICES.get().copied() else {
        hyper::debug::invariant_failure("irq::kernel_rpc_services invariant")
    };
    services
}

/// Polls only the lock-safe mailbox service while local IRQs cannot run.
pub(crate) fn service_kernel_rpc() {
    (kernel_rpc_services().poll)();
}

/// Enters kernel policy after an architecture-private doorbell is acknowledged.
pub(crate) fn service_kernel_rpc_interrupt(origin: InterruptOrigin) -> EntryAction {
    (kernel_rpc_services().interrupt)(origin)
}
