// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host exception entry, interrupt domains, and cross-call transport.
//!
//! Device and timer subsystems own their interrupt handlers and lifecycle;
//! this module supplies the controller, routing, and dispatch mechanisms they
//! consume.

use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, Ordering};

pub(crate) mod cross_call;
pub(crate) mod exception;
pub mod interrupt;
pub(crate) mod reschedule;

pub use interrupt::acknowledge_external;

static EXCEPTIONS_READY: AtomicBool = AtomicBool::new(false);
type EntryReadyLock =
    InterruptSpinLock<Option<crate::hal::exception::EntryReady>, crate::hal::irq::LocalMask>;
static ENTRY_READY: EntryReadyLock = InterruptSpinLock::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    Controller(interrupt::Error),
    EntryServices(crate::hal::exception::EntryServiceError),
    EntryReadyAlreadyPublished,
    KernelRpcService(crate::hal::irq::KernelRpcServiceError),
    MissingController,
    RuntimeVectors(crate::hal::exception::RuntimeVectorError),
    Reschedule(reschedule::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalInitializationError {
    Controller(interrupt::Error),
    MissingEntryServices,
    RuntimeVectors(crate::hal::exception::RuntimeVectorError),
}

/// Initializes host interrupt delivery before interrupt consumers are started.
///
/// The scheduler already owns its per-CPU pending state at this point. This
/// subsystem owns the transport lifecycle: controller publication, runtime
/// exception entry, and the permanent reschedule cross-call mapping. Local
/// interrupts remain masked and no secondary CPU is admitted until all three
/// stages are ready.
pub(crate) fn initialize(
    boot: &mut crate::boot::Initialization,
) -> Result<(), InitializationError> {
    initialize_controller(boot)?;
    crate::hal::irq::install_kernel_rpc_services(
        cross_call::service,
        crate::entry::irq::dispatch_kernel_rpc_entry,
    )
    .map_err(InitializationError::KernelRpcService)?;
    interrupt::initialize_local_rpc_transport().map_err(InitializationError::Controller)?;
    let entry = crate::hal::exception::install_entry_services(
        crate::entry::exception::fatal,
        crate::entry::irq::dispatch,
        crate::entry::irq::claim_and_dispatch_external,
        crate::entry::irq::stop,
    )
    .map_err(InitializationError::EntryServices)?;
    initialize_exceptions(&entry)?;
    ENTRY_READY.with(|slot| {
        if slot.is_some() {
            Err(InitializationError::EntryReadyAlreadyPublished)
        } else {
            *slot = Some(entry);
            Ok(())
        }
    })?;
    reschedule::initialize(boot.interrupts().root_domain).map_err(InitializationError::Reschedule)
}

/// Observes reschedule-SGI dispatch for the bare-metal runtime proof.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[allow(
    dead_code,
    reason = "IPI delivery counters are exercised by AArch64 and x86 runtime tests"
)]
pub(crate) fn reschedule_delivery_count_for_test(cpu: hyper::cpu::CpuIndex) -> usize {
    reschedule::delivery_count_for_test(cpu)
}

/// Initializes the root interrupt controller and kernel IRQ domain.
fn initialize_controller(
    boot: &mut crate::boot::Initialization,
) -> Result<(), InitializationError> {
    let info = boot
        .essential()
        .interrupt_controller()
        .ok_or(InitializationError::MissingController)?;
    let capabilities = interrupt::initialize(info).map_err(InitializationError::Controller)?;
    crate::pr_info!(
        "HypeR: interrupt controller initialized with {} interrupt IDs; local IRQs remain masked",
        capabilities.interrupt_count
    );
    boot.set_interrupts(capabilities);
    Ok(())
}

/// Installs and validates the permanent runtime exception vectors.
fn initialize_exceptions(
    entry: &crate::hal::exception::EntryReady,
) -> Result<(), InitializationError> {
    // SAFETY: The final RX kernel mapping, stack, console, and interrupt
    // controller are active. IRQ delivery remains masked until runtime activation.
    unsafe { crate::hal::exception::install_runtime_vectors(entry) };
    crate::hal::exception::validate_runtime_vectors()
        .map_err(InitializationError::RuntimeVectors)?;
    EXCEPTIONS_READY.store(true, Ordering::Release);
    Ok(())
}

/// Installs exception entry and interrupt-controller state local to a secondary CPU.
///
/// The global vector table, IRQ domain, and controller are immutable before
/// SMP admission. Local interrupts remain masked throughout this operation.
pub(crate) fn initialize_local_cpu() -> Result<(), LocalInitializationError> {
    let entry = ENTRY_READY
        .with(|slot| *slot)
        .ok_or(LocalInitializationError::MissingEntryServices)?;
    // SAFETY: SMP prepared this CPU's pinned exception stacks before CPU_ON;
    // the permanent executable mapping is active and IRQs remain masked.
    unsafe { crate::hal::exception::install_local_runtime_vectors(&entry) };
    crate::hal::exception::validate_local_runtime_vectors()
        .map_err(LocalInitializationError::RuntimeVectors)?;
    interrupt::initialize_local_cpu().map_err(LocalInitializationError::Controller)
}

pub(crate) fn exceptions_ready() -> bool {
    EXCEPTIONS_READY.load(Ordering::Acquire)
}
