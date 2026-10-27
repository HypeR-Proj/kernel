// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! IRQ domains, mappings, registration, and dispatch policy.

use alloc::vec::Vec;
use core::cell::UnsafeCell;

use hyper::cpu::PerCpu;
use hyper::hal::interrupt::{
    InterruptController, InterruptId, InterruptPriority, InterruptTransitionError,
    InterruptTrigger, KernelInterruptController, LocalInterruptController,
};
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicBool, Ordering};
use {hyper_platform::InterruptControllerInfo, hyper_platform::PlatformInterrupt};

type BootInterruptController = crate::hal::irq::Controller;
type InterruptLock = InterruptSpinLock<Option<InterruptState>, crate::hal::irq::LocalMask>;
type LocalController = <BootInterruptController as KernelInterruptController>::Local;

struct LocalControllerSlot(UnsafeCell<Option<LocalController>>);

// SAFETY: Each slot is installed and accessed only by its matching logical
// CPU. The immutable participating topology has no hotplug or slot migration.
unsafe impl Sync for LocalControllerSlot {}

impl LocalControllerSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn install(&self, controller: LocalController) -> Result<(), Error> {
        // SAFETY: Only the executing CPU accesses its indexed slot and
        // installation occurs once before that CPU enables local interrupts.
        let slot = unsafe { &mut *self.0.get() };
        if slot.is_some() {
            return Err(Error::AlreadyInitialized);
        }
        *slot = Some(controller);
        Ok(())
    }

    fn get(&self) -> Option<&LocalController> {
        // SAFETY: The current CPU is the only accessor of its immutable slot;
        // installation completed before online Release publication.
        unsafe { (&*self.0.get()).as_ref() }
    }
}

const UNHANDLED_QUARANTINE_THRESHOLD: u32 = 8;

static INTERRUPTS: InterruptLock = InterruptSpinLock::new(None);
static LOCAL_CONTROLLERS: PerCpu<LocalControllerSlot> =
    PerCpu::new([const { LocalControllerSlot::new() }; hyper::cpu::MAX_CPUS]);
static LOCAL_LIFECYCLE_TRANSACTION: AtomicBool = AtomicBool::new(false);

struct LocalLifecycleTransaction;

impl LocalLifecycleTransaction {
    fn acquire() -> Result<Self, Error> {
        LOCAL_LIFECYCLE_TRANSACTION
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| Self)
            .map_err(|_| Error::LocalInterruptLifecycleBusy)
    }
}

impl Drop for LocalLifecycleTransaction {
    fn drop(&mut self) {
        LOCAL_LIFECYCLE_TRANSACTION.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    AlreadyInitialized,
    BootPhaseOnly,
    DomainBusy,
    DomainNotFound,
    Controller(crate::hal::irq::ControllerError),
    HandlerNotFound,
    InterruptAlreadyMapped,
    InterruptNotMapped,
    LocalInterruptLifecycleRequiresCrossCall,
    LocalInterruptLifecycleBusy,
    LocalInterruptCrossCallFailed(usize),
    LocalInterruptLifecyclePoisoned,
    MappingBusy,
    NotInitialized,
    NumericExhaustion,
    ReservedInterrupt,
}

impl From<crate::hal::irq::ControllerError> for Error {
    fn from(error: crate::hal::irq::ControllerError) -> Self {
        Self::Controller(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqDomainId(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualInterrupt(u32);

impl VirtualInterrupt {
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Exclusive capability to remove one registered IRQ handler.
///
/// Dropping an armed capability is a fatal ownership violation. `Drop` never
/// acquires the IRQ registry lock or changes hardware state because destruction
/// may occur while an unrelated lock is held. Its owner must either pass it to
/// [`unregister`] or explicitly call [`Registration::retain_permanently`] for a
/// kernel-lifetime handler.
#[must_use = "an IRQ registration must be owned, unregistered, or explicitly retained permanently"]
#[derive(Debug, Eq, PartialEq)]
pub struct Registration {
    id: u64,
    interrupt: VirtualInterrupt,
    armed: bool,
}

#[must_use = "a prepared IRQ mapping must be activated or explicitly discarded"]
#[derive(Debug, Eq, PartialEq)]
pub struct PreparedRegistration {
    registration: Registration,
}

impl PreparedRegistration {
    pub const fn interrupt(&self) -> VirtualInterrupt {
        self.registration.interrupt
    }

    fn disarm(&mut self) {
        self.registration.disarm();
    }
}

#[must_use = "failed IRQ activation preserves ownership of the prepared mapping"]
pub struct ActivationFailure {
    error: Error,
    prepared: PreparedRegistration,
}

#[must_use = "failed IRQ preparation discard returns ownership of the prepared mapping"]
pub struct DiscardFailure {
    error: Error,
    prepared: PreparedRegistration,
}

impl DiscardFailure {
    pub fn into_parts(self) -> (Error, PreparedRegistration) {
        (self.error, self.prepared)
    }
}

impl ActivationFailure {
    pub fn into_parts(self) -> (Error, PreparedRegistration) {
        (self.error, self.prepared)
    }
}

impl Registration {
    const fn new(id: u64, interrupt: VirtualInterrupt) -> Self {
        Self {
            id,
            interrupt,
            armed: true,
        }
    }

    pub(crate) const fn interrupt(&self) -> VirtualInterrupt {
        self.interrupt
    }

    fn disarm(&mut self) {
        if !self.armed {
            hyper::debug::invariant_failure("irq::interrupt::disarm invariant")
        }
        self.armed = false;
    }

    /// Relinquishes the unregister capability for a kernel-lifetime handler.
    ///
    /// This is intentionally explicit and irreversible. The registry owns the
    /// handler entry until shutdown, and `HypeR` currently has no global IRQ
    /// teardown phase. Subsystems with a shorter lifetime must store this
    /// capability in their owning state instead.
    pub fn retain_permanently(mut self) {
        self.disarm();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.armed {
            // Destruction can run under an arbitrary lock and interrupt-mask
            // state. Do not release callbacks or use ordinary logs; the panic handler
            // reports without unwinding the live callback owner.
            hyper::debug::invariant_failure("irq::interrupt::drop invariant")
        }
    }
}

/// An unsuccessful unregister operation that preserves its capability.
///
/// A valid registration remains installed when unregistration fails. The
/// caller can retry with the returned capability or explicitly retain the
/// handler permanently if its failure path cannot safely retry.
#[must_use = "failed IRQ unregistration returns ownership of the still-active registration"]
#[derive(Debug, Eq, PartialEq)]
pub struct UnregisterFailure {
    error: Error,
    registration: Registration,
}

impl UnregisterFailure {
    pub fn into_parts(self) -> (Error, Registration) {
        (self.error, self.registration)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandlerResult {
    Handled,
    NotHandled,
    HandledAndMaskLocal,
    HandledAndUnmaskLocal(VirtualInterrupt),
}

/// Synchronous IRQ handler. The frame view is CPU-local and cannot be retained.
pub type Handler =
    fn(VirtualInterrupt, usize, &crate::hal::irq::InterruptFrame<'_>) -> HandlerResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub interrupt_count: u32,
    pub root_domain: IrqDomainId,
    pub maintenance_interrupt: Option<PlatformInterrupt>,
}

struct HandlerEntry {
    id: u64,
    context: usize,
    handler: Handler,
}

struct IrqMapping {
    hardware: InterruptId,
    virtual_interrupt: VirtualInterrupt,
    priority: InterruptPriority,
    trigger: InterruptTrigger,
    handlers: Vec<HandlerEntry>,
    /// Registry intent shared by all CPUs. Per-CPU quarantine must not clear it.
    enabled_by_registry: bool,
    consecutive_unhandled: u32,
    lifecycle: MappingLifecycle,
}

impl IrqMapping {
    fn dispatch_handlers(
        &mut self,
        hardware: InterruptId,
        per_cpu: bool,
        frame: &crate::hal::irq::InterruptFrame<'_>,
    ) -> HandlerDispatch {
        let mut actions = HandlerActions::default();
        for entry in &self.handlers {
            actions.record((entry.handler)(
                self.virtual_interrupt,
                entry.context,
                frame,
            ));
        }

        let outcome = if actions.handled {
            self.consecutive_unhandled = 0;
            DispatchOutcome::Handled
        } else {
            self.consecutive_unhandled = self.consecutive_unhandled.saturating_add(1);
            if self.consecutive_unhandled >= UNHANDLED_QUARANTINE_THRESHOLD
                && self.enabled_by_registry
            {
                // A PPI is disabled only on this CPU. Preserve the registry's
                // installed intent so secondary-CPU replay and an explicit
                // local re-enable remain correct.
                if !per_cpu {
                    self.enabled_by_registry = false;
                }
                DispatchOutcome::Quarantined {
                    hardware,
                    virtual_interrupt: self.virtual_interrupt,
                }
            } else {
                DispatchOutcome::Handled
            }
        };
        let quarantine = matches!(outcome, DispatchOutcome::Quarantined { .. });
        HandlerDispatch {
            outcome,
            quarantine,
            mask_local: actions.mask_local,
            unmask_local: actions.unmask_local,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingLifecycle {
    Prepared,
    Enabling,
    Active,
    Disabling,
}

use super::cross_call::LocalIrqOperation as LocalLifecycleOperation;

struct IrqDomain {
    id: IrqDomainId,
    mappings: Vec<IrqMapping>,
}

struct InterruptState {
    controller: BootInterruptController,
    domains: Vec<IrqDomain>,
    next_domain: u32,
    next_virtual_interrupt: u32,
    next_registration: u64,
}

enum DispatchOutcome {
    Handled,
    Prepared {
        hardware: InterruptId,
        virtual_interrupt: VirtualInterrupt,
    },
    Quarantined {
        hardware: InterruptId,
        virtual_interrupt: VirtualInterrupt,
    },
    Unmapped(InterruptId),
    TransitionAmbiguous {
        operation: &'static str,
        error: crate::hal::irq::ControllerError,
    },
}

#[derive(Default)]
struct HandlerActions {
    handled: bool,
    mask_local: bool,
    unmask_local: Option<VirtualInterrupt>,
}

impl HandlerActions {
    fn record(&mut self, result: HandlerResult) {
        match result {
            HandlerResult::Handled => self.handled = true,
            HandlerResult::NotHandled => {}
            HandlerResult::HandledAndMaskLocal => {
                self.handled = true;
                self.mask_local = true;
            }
            HandlerResult::HandledAndUnmaskLocal(interrupt) => {
                self.handled = true;
                self.unmask_local = Some(interrupt);
            }
        }
    }
}

struct HandlerDispatch {
    outcome: DispatchOutcome,
    quarantine: bool,
    mask_local: bool,
    unmask_local: Option<VirtualInterrupt>,
}

enum TransitionFailure {
    NotApplied(Error),
    AppliedOrUnknown(crate::hal::irq::ControllerError),
}

impl From<Error> for TransitionFailure {
    fn from(error: Error) -> Self {
        Self::NotApplied(error)
    }
}

fn classify_transition(
    result: Result<(), InterruptTransitionError<crate::hal::irq::ControllerError>>,
) -> Result<(), TransitionFailure> {
    result.map_err(|error| match error {
        InterruptTransitionError::NotApplied(error) => {
            TransitionFailure::NotApplied(Error::Controller(error))
        }
        InterruptTransitionError::AppliedOrUnknown(error) => {
            TransitionFailure::AppliedOrUnknown(error)
        }
    })
}

fn resolve_transition<ResultValue>(
    result: Result<ResultValue, TransitionFailure>,
    operation: &'static str,
) -> Result<ResultValue, Error> {
    match result {
        Ok(value) => Ok(value),
        Err(TransitionFailure::NotApplied(error)) => Err(error),
        Err(TransitionFailure::AppliedOrUnknown(error)) => {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: interrupt-controller {operation} reached an ambiguous commit state: {error:?}"
            ))
        }
    }
}

fn dispatch_transition(
    result: Result<(), TransitionFailure>,
    operation: &'static str,
) -> Option<DispatchOutcome> {
    match result {
        Ok(()) | Err(TransitionFailure::NotApplied(_)) => None,
        Err(TransitionFailure::AppliedOrUnknown(error)) => {
            Some(DispatchOutcome::TransitionAmbiguous { operation, error })
        }
    }
}

pub fn initialize(info: InterruptControllerInfo) -> Result<Capabilities, Error> {
    if INTERRUPTS.with(|slot| slot.is_some()) {
        return Err(Error::AlreadyInitialized);
    }
    let maintenance_interrupt = match info {
        InterruptControllerInfo::GicV3(info) => info.maintenance_interrupt,
        InterruptControllerInfo::GicV2(info) => info.maintenance_interrupt,
        InterruptControllerInfo::Plic(_) => None,
        InterruptControllerInfo::X2Apic(_) => None,
    };
    let root_domain = IrqDomainId(0);
    let mut domains = Vec::new();
    reserve_one(&mut domains)?;
    domains.push(IrqDomain {
        id: root_domain,
        mappings: Vec::new(),
    });
    // SAFETY: The architecture MMIO window maps every DTB-discovered device
    // range with Device attributes and the controller has a single owner.
    let controller =
        unsafe { BootInterruptController::bind(info, crate::mm::memory::mmio_address)? };
    let interrupt_count = controller.interrupt_count();
    // The root domain and all fallible storage are complete before this single
    // publication. Consumers can therefore never observe a controller without
    // the domain capability returned below.
    INTERRUPTS.with(|slot| {
        if slot.is_some() {
            return Err(Error::AlreadyInitialized);
        }
        *slot = Some(InterruptState {
            controller,
            domains,
            next_domain: 1,
            next_virtual_interrupt: 0,
            next_registration: 0,
        });
        Ok(())
    })?;
    Ok(Capabilities {
        interrupt_count,
        root_domain,
        maintenance_interrupt,
    })
}

/// Initializes the calling secondary CPU's local interrupt-controller state.
pub fn initialize_local_cpu() -> Result<(), Error> {
    let cpu = crate::cpu::current_index().ok_or(Error::NotInitialized)?;
    let initialized = with_transition_state(|state| {
        let InterruptState {
            controller,
            domains,
            ..
        } = state;
        // SAFETY: The shared Distributor is active, this CPU still has IRQs
        // masked, and each Redistributor is private to its matching affinity.
        let local = unsafe { controller.initialize_local().map_err(Error::from)? };
        LOCAL_CONTROLLERS[cpu].install(local)?;
        let local = LOCAL_CONTROLLERS[cpu].get().ok_or(Error::NotInitialized)?;
        for mapping in domains.iter().flat_map(|domain| domain.mappings.iter()) {
            if controller.is_per_cpu(mapping.hardware) {
                local
                    .configure(mapping.hardware, mapping.priority, mapping.trigger)
                    .map_err(Error::from)?;
                if mapping.enabled_by_registry {
                    classify_transition(local.enable(mapping.hardware))?;
                }
            }
        }
        Ok(())
    });
    resolve_transition(initialized, "secondary local-route replay")?;
    initialize_local_rpc_transport()
}

pub(crate) fn initialize_local_rpc_transport() -> Result<(), Error> {
    let cpu = crate::cpu::current_index().ok_or(Error::NotInitialized)?;
    if LOCAL_CONTROLLERS[cpu].get().is_none() {
        let local = with_state(|state| state.controller.local_controller().map_err(Error::from))?;
        LOCAL_CONTROLLERS[cpu].install(local)?;
    }
    crate::hal::irq::arm_kernel_rpc_source();
    let Some(interrupt) = crate::hal::irq::kernel_rpc_interrupt() else {
        return Ok(());
    };
    {
        let controller = LOCAL_CONTROLLERS[cpu].get().ok_or(Error::NotInitialized)?;
        controller
            .configure(
                interrupt,
                InterruptPriority::Critical,
                InterruptTrigger::Edge,
            )
            .map_err(Error::from)?;
        resolve_transition(
            classify_transition(controller.enable(interrupt)),
            "kernel RPC source enable",
        )
    }
}

pub fn create_domain() -> Result<IrqDomainId, Error> {
    with_state(|state| {
        reserve_one(&mut state.domains)?;
        let id = IrqDomainId(state.next_domain);
        state.next_domain = state
            .next_domain
            .checked_add(1)
            .ok_or(Error::NumericExhaustion)?;
        state.domains.push(IrqDomain {
            id,
            mappings: Vec::new(),
        });
        Ok(id)
    })
}

/// Destroys an IRQ domain after all of its mappings have been removed.
pub fn destroy_domain(domain: IrqDomainId) -> Result<(), Error> {
    with_state(|state| {
        let index = state
            .domains
            .iter()
            .position(|candidate| candidate.id == domain)
            .ok_or(Error::DomainNotFound)?;
        if !state.domains[index].mappings.is_empty() {
            return Err(Error::DomainBusy);
        }
        state.domains.swap_remove(index);
        Ok(())
    })
}

/// Creates a controller mapping and allocates a kernel-global virtual IRQ.
pub fn map(
    domain: IrqDomainId,
    hardware: InterruptId,
    priority: InterruptPriority,
    trigger: InterruptTrigger,
) -> Result<VirtualInterrupt, Error> {
    reject_reserved_interrupt(hardware)?;
    with_state(|state| {
        if state
            .domains
            .iter()
            .flat_map(|domain| domain.mappings.iter())
            .any(|mapping| mapping.hardware == hardware)
        {
            return Err(Error::InterruptAlreadyMapped);
        }
        let domain_index = state
            .domains
            .iter()
            .position(|candidate| candidate.id == domain)
            .ok_or(Error::DomainNotFound)?;
        let next_virtual_interrupt = state
            .next_virtual_interrupt
            .checked_add(1)
            .ok_or(Error::NumericExhaustion)?;
        reserve_one(&mut state.domains[domain_index].mappings)?;
        state.controller.configure(hardware, priority, trigger)?;
        let virtual_interrupt = VirtualInterrupt(state.next_virtual_interrupt);
        state.next_virtual_interrupt = next_virtual_interrupt;
        state.domains[domain_index].mappings.push(IrqMapping {
            hardware,
            virtual_interrupt,
            priority,
            trigger,
            handlers: Vec::new(),
            enabled_by_registry: false,
            consecutive_unhandled: 0,
            lifecycle: MappingLifecycle::Prepared,
        });
        Ok(virtual_interrupt)
    })
}

impl IrqDomainId {
    pub(crate) const fn diagnostic_id(self) -> u32 {
        self.0
    }

    /// Publishes a disabled mapping and handler context, then configures every
    /// participating CPU without making the source deliverable.
    ///
    /// The source must be quiesced and pending state cleared before this call.
    /// Additional handler dependencies may be published afterward, but their
    /// Release publication must complete before [`activate`] is called.
    pub fn prepare_shared_mapping(
        self,
        hardware: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
        context: usize,
        handler: Handler,
    ) -> Result<PreparedRegistration, Error> {
        reject_reserved_interrupt(hardware)?;
        let _transaction = LocalLifecycleTransaction::acquire()?;
        let (mut prepared, late) = with_state(|state| {
            if state
                .domains
                .iter()
                .flat_map(|domain| domain.mappings.iter())
                .any(|mapping| mapping.hardware == hardware)
            {
                return Err(Error::InterruptAlreadyMapped);
            }
            let domain_index = state
                .domains
                .iter()
                .position(|candidate| candidate.id == self)
                .ok_or(Error::DomainNotFound)?;
            let next_virtual_interrupt = state
                .next_virtual_interrupt
                .checked_add(1)
                .ok_or(Error::NumericExhaustion)?;
            let next_registration = state
                .next_registration
                .checked_add(1)
                .ok_or(Error::NumericExhaustion)?;
            reserve_one(&mut state.domains[domain_index].mappings)?;
            let mut handlers = Vec::new();
            reserve_one(&mut handlers)?;
            let late = late_local_lifecycle_required(&state.controller, hardware);
            if !late {
                state.controller.configure(hardware, priority, trigger)?;
            }
            let virtual_interrupt = VirtualInterrupt(state.next_virtual_interrupt);
            let registration = Registration::new(state.next_registration, virtual_interrupt);
            handlers.push(HandlerEntry {
                id: registration.id,
                context,
                handler,
            });
            state.next_virtual_interrupt = next_virtual_interrupt;
            state.next_registration = next_registration;
            state.domains[domain_index].mappings.push(IrqMapping {
                hardware,
                virtual_interrupt,
                priority,
                trigger,
                handlers,
                enabled_by_registry: false,
                consecutive_unhandled: 0,
                lifecycle: MappingLifecycle::Prepared,
            });
            Ok((PreparedRegistration { registration }, late))
        })?;
        if late
            && let Err(error) = synchronize_local_lifecycle(
                hardware,
                priority,
                trigger,
                LocalLifecycleOperation::Configure,
            )
        {
            if let Err(rollback) = remove_prepared_mapping(&prepared) {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: prepared IRQ configuration rollback failed: {rollback:?}"
                ));
            }
            prepared.disarm();
            return Err(error);
        }
        Ok(prepared)
    }

    /// Creates a mapping and installs its first shared handler transactionally.
    ///
    /// All validation, allocation, and controller operations complete while
    /// the registry is locked. The populated mapping is published only by the
    /// final infallible insertion.
    ///
    /// This is a boot-phase installation interface. The caller must keep local
    /// IRQs masked and the device source quiesced until it has published every
    /// state object the handler reads. Enabling the controller route here does
    /// not make an unpublished handler context safe to observe.
    pub fn register_shared_mapping(
        self,
        hardware: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
        context: usize,
        handler: Handler,
    ) -> Result<(VirtualInterrupt, Registration), Error> {
        reject_reserved_interrupt(hardware)?;
        if crate::cpu::frozen_topology().is_some() {
            return Err(Error::BootPhaseOnly);
        }
        let installed = with_transition_state(|state| {
            if state
                .domains
                .iter()
                .flat_map(|domain| domain.mappings.iter())
                .any(|mapping| mapping.hardware == hardware)
            {
                return Err(TransitionFailure::NotApplied(Error::InterruptAlreadyMapped));
            }
            let domain_index = state
                .domains
                .iter()
                .position(|candidate| candidate.id == self)
                .ok_or(Error::DomainNotFound)?;
            let next_virtual_interrupt = state
                .next_virtual_interrupt
                .checked_add(1)
                .ok_or(Error::NumericExhaustion)?;
            let next_registration = state
                .next_registration
                .checked_add(1)
                .ok_or(Error::NumericExhaustion)?;
            reserve_one(&mut state.domains[domain_index].mappings)?;
            let mut handlers = Vec::new();
            reserve_one(&mut handlers)?;
            require_local_lifecycle_available(&state.controller, hardware)?;
            state
                .controller
                .configure(hardware, priority, trigger)
                .map_err(Error::from)?;
            classify_transition(state.controller.enable(hardware))?;

            let virtual_interrupt = VirtualInterrupt(state.next_virtual_interrupt);
            let registration = Registration::new(state.next_registration, virtual_interrupt);
            handlers.push(HandlerEntry {
                id: registration.id,
                context,
                handler,
            });
            state.next_virtual_interrupt = next_virtual_interrupt;
            state.next_registration = next_registration;
            state.domains[domain_index].mappings.push(IrqMapping {
                hardware,
                virtual_interrupt,
                priority,
                trigger,
                handlers,
                enabled_by_registry: true,
                consecutive_unhandled: 0,
                lifecycle: MappingLifecycle::Active,
            });
            Ok((virtual_interrupt, registration))
        });
        resolve_transition(installed, "boot IRQ mapping enable")
    }
}

#[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
pub(crate) fn route_prepared_to_cpu(
    prepared: &PreparedRegistration,
    cpu: hyper::cpu::CpuIndex,
) -> Result<(), Error> {
    with_state(|state| {
        let (domain, index) = state
            .mapping_position_by_virtual(prepared.registration.interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        let mapping = &state.domains[domain].mappings[index];
        if mapping.lifecycle != MappingLifecycle::Prepared {
            return Err(Error::MappingBusy);
        }
        state
            .controller
            .route_to_cpu(mapping.hardware, cpu)
            .map_err(Into::into)
    })
}

pub fn activate(prepared: PreparedRegistration) -> Result<Registration, ActivationFailure> {
    let _transaction = match LocalLifecycleTransaction::acquire() {
        Ok(transaction) => transaction,
        Err(error) => return Err(ActivationFailure { error, prepared }),
    };
    let details = with_state(|state| {
        let (domain, mapping) = state
            .mapping_position_by_virtual(prepared.registration.interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        let mapping = &mut state.domains[domain].mappings[mapping];
        if mapping.lifecycle != MappingLifecycle::Prepared {
            return Err(Error::MappingBusy);
        }
        mapping.lifecycle = MappingLifecycle::Enabling;
        Ok((
            mapping.hardware,
            mapping.priority,
            mapping.trigger,
            late_local_lifecycle_required(&state.controller, mapping.hardware),
        ))
    });
    let (hardware, priority, trigger, late) = match details {
        Ok(details) => details,
        Err(error) => return Err(ActivationFailure { error, prepared }),
    };
    let enabled = if late {
        synchronize_local_lifecycle(hardware, priority, trigger, LocalLifecycleOperation::Enable)
    } else {
        resolve_transition(
            with_transition_state(|state| {
                classify_transition(state.controller.enable(hardware))?;
                Ok(())
            }),
            "IRQ activation enable",
        )
    };
    if let Err(error) = enabled {
        if let Err(commit_error) = with_state(|state| {
            let (domain, mapping) = state
                .mapping_position_by_virtual(prepared.registration.interrupt)
                .ok_or(Error::InterruptNotMapped)?;
            state.domains[domain].mappings[mapping].lifecycle = MappingLifecycle::Prepared;
            Ok(())
        }) {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: failed IRQ activation rollback lost registry state: {commit_error:?}"
            ));
        }
        return Err(ActivationFailure { error, prepared });
    }
    let registration = prepared.registration;
    let committed = with_state(|state| {
        let (domain, mapping) = state
            .mapping_position_by_virtual(registration.interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        let mapping = &mut state.domains[domain].mappings[mapping];
        mapping.enabled_by_registry = true;
        mapping.lifecycle = MappingLifecycle::Active;
        Ok(())
    });
    if let Err(error) = committed {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: enabled IRQ activation lost registry state: {error:?}"
        ));
    }
    Ok(registration)
}

pub fn discard_prepared(mut prepared: PreparedRegistration) -> Result<(), DiscardFailure> {
    let _transaction = match LocalLifecycleTransaction::acquire() {
        Ok(transaction) => transaction,
        Err(error) => return Err(DiscardFailure { error, prepared }),
    };
    if let Err(error) = remove_prepared_mapping(&prepared) {
        return Err(DiscardFailure { error, prepared });
    }
    prepared.disarm();
    Ok(())
}

fn remove_prepared_mapping(prepared: &PreparedRegistration) -> Result<(), Error> {
    with_state(|state| {
        let (domain, mapping) = state
            .mapping_position_by_virtual(prepared.registration.interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        if state.domains[domain].mappings[mapping].lifecycle != MappingLifecycle::Prepared {
            return Err(Error::MappingBusy);
        }
        state.domains[domain].mappings.swap_remove(mapping);
        Ok(())
    })
}

/// Removes an unused mapping. Mappings with registered handlers stay pinned.
pub fn unmap(interrupt: VirtualInterrupt) -> Result<(), Error> {
    let _transaction = LocalLifecycleTransaction::acquire()?;
    with_state(|state| {
        let (domain_index, mapping_index) = state
            .mapping_position_by_virtual(interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        if !state.domains[domain_index].mappings[mapping_index]
            .handlers
            .is_empty()
            || state.domains[domain_index].mappings[mapping_index].lifecycle
                != MappingLifecycle::Prepared
        {
            return Err(Error::MappingBusy);
        }
        state.domains[domain_index]
            .mappings
            .swap_remove(mapping_index);
        Ok(())
    })
}

/// Adds one handler to a shared virtual IRQ.
///
/// Handlers run with local IRQs masked and the global registry lock held. They
/// must not allocate, block, mutate IRQ domains or registrations, or acquire a
/// lock ordered before the IRQ registry. Every shared handler is invoked so
/// devices sharing a level-triggered line can inspect and clear their own
/// interrupt source. `context` is an opaque value owned by the registering
/// driver and must remain valid until unregister.
pub fn register_shared(
    interrupt: VirtualInterrupt,
    context: usize,
    handler: Handler,
) -> Result<Registration, Error> {
    let installed = with_transition_state(|state| {
        let (domain_index, mapping_index) = state
            .mapping_position_by_virtual(interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        let next_registration = state
            .next_registration
            .checked_add(1)
            .ok_or(Error::NumericExhaustion)?;
        let mapping = &mut state.domains[domain_index].mappings[mapping_index];
        if !matches!(
            mapping.lifecycle,
            MappingLifecycle::Prepared | MappingLifecycle::Active
        ) || (mapping.lifecycle == MappingLifecycle::Prepared && !mapping.handlers.is_empty())
        {
            return Err(TransitionFailure::NotApplied(Error::MappingBusy));
        }
        reserve_one(&mut mapping.handlers)?;
        if !mapping.enabled_by_registry {
            require_local_lifecycle_available(&state.controller, mapping.hardware)?;
            classify_transition(state.controller.enable(mapping.hardware))?;
            mapping.enabled_by_registry = true;
            mapping.consecutive_unhandled = 0;
            mapping.lifecycle = MappingLifecycle::Active;
        }
        let registration = Registration::new(state.next_registration, interrupt);
        state.next_registration = next_registration;
        mapping.handlers.push(HandlerEntry {
            id: registration.id,
            context,
            handler,
        });
        Ok(registration)
    });
    resolve_transition(installed, "shared IRQ enable")
}

/// Removes exactly the handler identified by `registration`.
///
/// The hardware line is disabled automatically when its final handler is
/// removed. Failure leaves the handler installed and returns the exclusive
/// capability in [`UnregisterFailure`], allowing the owner to retry or retain
/// it deliberately. If this registration may be the final handler, the caller
/// must first quiesce the device source and clear its pending condition. The
/// controller disable prevents new delivery but cannot retract an already
/// latched edge before the same hardware interrupt is reused by another owner.
pub fn unregister(mut registration: Registration) -> Result<(), UnregisterFailure> {
    let _transaction = match LocalLifecycleTransaction::acquire() {
        Ok(transaction) => transaction,
        Err(error) => {
            return Err(UnregisterFailure {
                error,
                registration,
            });
        }
    };
    let prepared = with_transition_state(|state| {
        let (domain_index, mapping_index) = state
            .mapping_position_by_virtual(registration.interrupt)
            .ok_or(Error::InterruptNotMapped)?;
        let mapping = &mut state.domains[domain_index].mappings[mapping_index];
        if mapping.lifecycle != MappingLifecycle::Active {
            return Err(TransitionFailure::NotApplied(Error::MappingBusy));
        }
        let handler_index = mapping
            .handlers
            .iter()
            .position(|handler| handler.id == registration.id)
            .ok_or(Error::HandlerNotFound)?;
        let late = mapping.handlers.len() == 1
            && mapping.enabled_by_registry
            && late_local_lifecycle_required(&state.controller, mapping.hardware);
        if late {
            mapping.lifecycle = MappingLifecycle::Disabling;
            return Ok(Some((mapping.hardware, mapping.priority, mapping.trigger)));
        }
        if mapping.handlers.len() == 1 && mapping.enabled_by_registry {
            require_local_lifecycle_available(&state.controller, mapping.hardware)?;
            classify_transition(state.controller.disable(mapping.hardware))?;
            mapping.enabled_by_registry = false;
            mapping.lifecycle = MappingLifecycle::Prepared;
        }
        mapping.handlers.swap_remove(handler_index);
        Ok(None)
    });
    let details = match resolve_transition(prepared, "final IRQ handler disable") {
        Ok(details) => details,
        Err(error) => {
            return Err(UnregisterFailure {
                error,
                registration,
            });
        }
    };
    if let Some((hardware, priority, trigger)) = details {
        if let Err(error) = synchronize_local_lifecycle(
            hardware,
            priority,
            trigger,
            LocalLifecycleOperation::Disable,
        ) {
            if let Err(rollback) = with_state(|state| {
                let (domain, mapping) = state
                    .mapping_position_by_virtual(registration.interrupt)
                    .ok_or(Error::InterruptNotMapped)?;
                state.domains[domain].mappings[mapping].lifecycle = MappingLifecycle::Active;
                Ok(())
            }) {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: failed IRQ deactivation rollback lost registry state: {rollback:?}"
                ));
            }
            return Err(UnregisterFailure {
                error,
                registration,
            });
        }
        if let Err(error) = with_state(|state| {
            let (domain, mapping) = state
                .mapping_position_by_virtual(registration.interrupt)
                .ok_or(Error::InterruptNotMapped)?;
            let mapping = &mut state.domains[domain].mappings[mapping];
            let handler = mapping
                .handlers
                .iter()
                .position(|entry| entry.id == registration.id)
                .ok_or(Error::HandlerNotFound)?;
            mapping.handlers.swap_remove(handler);
            mapping.enabled_by_registry = false;
            mapping.lifecycle = MappingLifecycle::Prepared;
            Ok(())
        }) {
            // Hardware disable is already globally visible. Returning an
            // apparently retryable token would hide an ambiguous committed
            // state, so an impossible registry mismatch is fail-stop.
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: disabled IRQ removal lost registry state: {error:?}"
            ));
        }
    }
    registration.disarm();
    Ok(())
}

/// Dispatches an interrupt already acknowledged by architecture exception entry.
pub fn dispatch(hardware: InterruptId, frame: &crate::hal::irq::InterruptFrame<'_>) {
    let outcome = match with_state(|state| Ok(state.dispatch_one(hardware, frame))) {
        Ok(outcome) => outcome,
        Err(Error::NotInitialized) => {
            crate::irq::exception::fatal_interrupt("controller not initialized", None)
        }
        Err(error) => crate::irq::exception::fatal_interrupt_state(error),
    };
    match outcome {
        DispatchOutcome::Handled => {}
        DispatchOutcome::Prepared {
            hardware,
            virtual_interrupt,
        } => crate::pr_warn!(
            "HypeR: masked IRQ delivered before activation: INTID {}, VIRQ {}",
            hardware.get(),
            virtual_interrupt.get()
        ),
        DispatchOutcome::Quarantined {
            hardware,
            virtual_interrupt,
        } => {
            crate::pr_warn!(
                "HypeR: quarantined unhandled IRQ: INTID {}, VIRQ {}",
                hardware.get(),
                virtual_interrupt.get()
            );
        }
        DispatchOutcome::Unmapped(hardware) => {
            crate::pr_warn!(
                "HypeR: masked IRQ without a domain mapping: INTID {}",
                hardware.get()
            );
        }
        DispatchOutcome::TransitionAmbiguous { operation, error } => {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: IRQ dispatch {operation} reached an ambiguous commit state: {error:?}"
            ));
        }
    }
}

/// Completes an acknowledged interrupt without invoking its registered handlers.
///
/// Fatal entry uses this for the reserved crash-stop source. Keeping completion
/// behind the controller owner gives every registered entry callback one
/// completion point, independent of the selected architecture's acknowledge
/// mechanism.
pub(crate) fn complete(hardware: InterruptId) {
    if let Err(error) = with_state(|state| {
        state.controller.end(hardware);
        Ok(())
    }) {
        crate::irq::exception::fatal_interrupt_state(error)
    }
}

/// Claims one pending source from a memory-mapped controller CPU context.
/// Architecture trap entry uses this only for external-interrupt causes.
pub fn acknowledge_external() -> Option<InterruptId> {
    with_state(|state| Ok(state.controller.acknowledge()))
        .ok()
        .flatten()
}

/// Enables one already-mapped PPI on the calling CPU.
pub fn enable_local(interrupt: VirtualInterrupt) -> Result<(), Error> {
    resolve_transition(
        with_transition_state(|state| state.set_local_enabled(interrupt, true)),
        "local IRQ enable",
    )
}

/// Re-enables one owned shared line after its device condition was acknowledged.
/// The live registration prevents removal/reuse while this operation runs.
/// Call outside device locks: callbacks run under the IRQ registry lock.
pub(crate) fn enable_registered_shared(registration: &Registration) -> Result<(), Error> {
    enable_registered_shared_if(registration, || true)
}

/// The condition executes under the IRQ registry lock, after any callback's
/// mask was applied, and must not call IRQ registry APIs or block.
pub(crate) fn enable_registered_shared_if(
    registration: &Registration,
    ready: impl FnOnce() -> bool,
) -> Result<(), Error> {
    resolve_transition(
        with_transition_state(|state| {
            let (domain, index) = state
                .mapping_position_by_virtual(registration.interrupt)
                .ok_or(Error::InterruptNotMapped)?;
            let mapping = &state.domains[domain].mappings[index];
            if mapping.lifecycle != MappingLifecycle::Active
                || !mapping
                    .handlers
                    .iter()
                    .any(|handler| handler.id == registration.id)
            {
                return Err(Error::MappingBusy.into());
            }
            let hardware = mapping.hardware;
            if state.controller.is_per_cpu(hardware) {
                return Err(Error::LocalInterruptLifecycleRequiresCrossCall.into());
            }
            if !ready() {
                return Ok(());
            }
            state.set_hardware_enabled(hardware, true)
        }),
        "owned shared IRQ enable",
    )
}

/// Disables one already-mapped PPI on the calling CPU.
pub fn disable_local(interrupt: VirtualInterrupt) -> Result<(), Error> {
    resolve_transition(
        with_transition_state(|state| state.set_local_enabled(interrupt, false)),
        "local IRQ disable",
    )
}

impl InterruptState {
    fn mapping_position_by_virtual(&self, interrupt: VirtualInterrupt) -> Option<(usize, usize)> {
        self.domains
            .iter()
            .enumerate()
            .find_map(|(domain, entries)| {
                entries
                    .mappings
                    .iter()
                    .position(|mapping| mapping.virtual_interrupt == interrupt)
                    .map(|mapping| (domain, mapping))
            })
    }

    fn mapping_position_by_hardware(&self, interrupt: InterruptId) -> Option<(usize, usize)> {
        self.domains
            .iter()
            .enumerate()
            .find_map(|(domain, entries)| {
                entries
                    .mappings
                    .iter()
                    .position(|mapping| mapping.hardware == interrupt)
                    .map(|mapping| (domain, mapping))
            })
    }

    fn dispatch_one(
        &mut self,
        hardware: InterruptId,
        frame: &crate::hal::irq::InterruptFrame<'_>,
    ) -> DispatchOutcome {
        let outcome = match self.mapping_position_by_hardware(hardware) {
            None => self.dispatch_unmapped_source(hardware),
            Some((domain, mapping)) => match self.domains[domain].mappings[mapping].lifecycle {
                MappingLifecycle::Prepared => {
                    self.dispatch_prepared_source(hardware, domain, mapping)
                }
                MappingLifecycle::Enabling
                | MappingLifecycle::Active
                | MappingLifecycle::Disabling => {
                    self.dispatch_mapped_source(hardware, domain, mapping, frame)
                }
            },
        };
        self.controller.end(hardware);
        outcome
    }

    fn dispatch_unmapped_source(&mut self, hardware: InterruptId) -> DispatchOutcome {
        dispatch_transition(
            classify_transition(self.controller.disable(hardware)),
            "unmapped-source disable",
        )
        .unwrap_or(DispatchOutcome::Unmapped(hardware))
    }

    fn dispatch_prepared_source(
        &mut self,
        hardware: InterruptId,
        domain: usize,
        mapping: usize,
    ) -> DispatchOutcome {
        let virtual_interrupt = self.domains[domain].mappings[mapping].virtual_interrupt;
        dispatch_transition(
            self.set_hardware_enabled(hardware, false),
            "prepared-source disable",
        )
        .unwrap_or(DispatchOutcome::Prepared {
            hardware,
            virtual_interrupt,
        })
    }

    fn dispatch_mapped_source(
        &mut self,
        hardware: InterruptId,
        domain: usize,
        mapping: usize,
        frame: &crate::hal::irq::InterruptFrame<'_>,
    ) -> DispatchOutcome {
        let per_cpu = self.controller.is_per_cpu(hardware);
        let dispatch =
            self.domains[domain].mappings[mapping].dispatch_handlers(hardware, per_cpu, frame);
        match self.apply_handler_actions(hardware, &dispatch) {
            Some(outcome) => outcome,
            None => dispatch.outcome,
        }
    }

    fn apply_handler_actions(
        &mut self,
        hardware: InterruptId,
        dispatch: &HandlerDispatch,
    ) -> Option<DispatchOutcome> {
        if dispatch.quarantine
            && let Some(ambiguous) = dispatch_transition(
                self.set_hardware_enabled(hardware, false),
                "quarantined-source disable",
            )
        {
            return Some(ambiguous);
        }
        if dispatch.mask_local
            && let Some(ambiguous) = dispatch_transition(
                self.set_hardware_enabled(hardware, false),
                "handler-requested local disable",
            )
        {
            return Some(ambiguous);
        }
        if let Some(interrupt) = dispatch.unmask_local
            && let Some((domain, mapping)) = self.mapping_position_by_virtual(interrupt)
        {
            if self.domains[domain].mappings[mapping].lifecycle != MappingLifecycle::Active {
                return Some(DispatchOutcome::Handled);
            }
            let target = self.domains[domain].mappings[mapping].hardware;
            if self.controller.is_per_cpu(target)
                && let Some(ambiguous) = dispatch_transition(
                    self.set_hardware_enabled(target, true),
                    "handler-requested local enable",
                )
            {
                return Some(ambiguous);
            }
        }
        None
    }

    fn set_local_enabled(
        &mut self,
        interrupt: VirtualInterrupt,
        enabled: bool,
    ) -> Result<(), TransitionFailure> {
        let (domain, mapping) = self
            .mapping_position_by_virtual(interrupt)
            .ok_or(Error::InterruptNotMapped)
            .map_err(TransitionFailure::from)?;
        let hardware = self.domains[domain].mappings[mapping].hardware;
        if self.domains[domain].mappings[mapping].lifecycle != MappingLifecycle::Active {
            return Err(Error::MappingBusy.into());
        }
        if !self.controller.is_per_cpu(hardware) {
            return Err(Error::LocalInterruptLifecycleRequiresCrossCall.into());
        }
        self.set_hardware_enabled(hardware, enabled)
    }

    fn set_hardware_enabled(
        &mut self,
        hardware: InterruptId,
        enabled: bool,
    ) -> Result<(), TransitionFailure> {
        if !self.controller.is_per_cpu(hardware) {
            return if enabled {
                classify_transition(self.controller.enable(hardware))
            } else {
                classify_transition(self.controller.disable(hardware))
            };
        }
        let cpu = crate::cpu::current_index()
            .ok_or(Error::NotInitialized)
            .map_err(TransitionFailure::from)?;
        let controller = LOCAL_CONTROLLERS[cpu]
            .get()
            .ok_or(Error::NotInitialized)
            .map_err(TransitionFailure::from)?;
        if enabled {
            classify_transition(controller.enable(hardware))
        } else {
            classify_transition(controller.disable(hardware))
        }
    }
}

fn with_state<R>(
    operation: impl FnOnce(&mut InterruptState) -> Result<R, Error>,
) -> Result<R, Error> {
    INTERRUPTS.with(|slot| {
        let state = slot.as_mut().ok_or(Error::NotInitialized)?;
        operation(state)
    })
}

fn with_transition_state<R>(
    operation: impl FnOnce(&mut InterruptState) -> Result<R, TransitionFailure>,
) -> Result<R, TransitionFailure> {
    INTERRUPTS.with(|slot| {
        let state = slot
            .as_mut()
            .ok_or(TransitionFailure::NotApplied(Error::NotInitialized))?;
        operation(state)
    })
}

fn reserve_one<T>(entries: &mut Vec<T>) -> Result<(), Error> {
    entries.try_reserve(1).map_err(|_| Error::Allocation)
}

fn reject_reserved_interrupt(interrupt: InterruptId) -> Result<(), Error> {
    if crate::hal::irq::kernel_rpc_interrupt().is_some_and(|kernel_rpc| interrupt == kernel_rpc) {
        Err(Error::ReservedInterrupt)
    } else {
        Ok(())
    }
}

fn late_local_lifecycle_required(
    controller: &BootInterruptController,
    interrupt: InterruptId,
) -> bool {
    controller.is_per_cpu(interrupt) && crate::cpu::frozen_topology().is_some()
}

fn synchronize_local_lifecycle(
    hardware: InterruptId,
    priority: InterruptPriority,
    trigger: InterruptTrigger,
    operation: LocalLifecycleOperation,
) -> Result<(), Error> {
    let topology = crate::cpu::frozen_topology().ok_or(Error::LocalInterruptLifecycleBusy)?;
    let count = topology.count();
    let targets = [true; hyper::cpu::MAX_CPUS];
    let forward = super::cross_call::execute(
        super::cross_call::KernelRpc::LocalIrqLifecycle {
            hardware,
            priority,
            trigger,
            operation,
        },
        count,
        &targets,
    )
    .map_err(|_| Error::LocalInterruptLifecycleBusy)?;
    if let Some(cpu) = forward.ambiguous_cpu {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: local IRQ lifecycle operation reached an ambiguous commit state on CPU {cpu}"
        ));
    }
    let Some(rejected) = forward.rejected_cpu else {
        return Ok(());
    };
    let inverse = match operation {
        LocalLifecycleOperation::Configure => None,
        LocalLifecycleOperation::Enable => Some(LocalLifecycleOperation::Disable),
        LocalLifecycleOperation::Disable => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: replicated-local IRQ disable was rejected; exact per-CPU mask state cannot be restored"
        )),
    };
    if let Some(operation) = inverse {
        let rollback = super::cross_call::execute(
            super::cross_call::KernelRpc::LocalIrqLifecycle {
                hardware,
                priority,
                trigger,
                operation,
            },
            count,
            &targets,
        )
        .map_err(|_| Error::LocalInterruptLifecyclePoisoned)?;
        if let Some(cpu) = rollback.ambiguous_cpu {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: local IRQ lifecycle rollback reached an ambiguous commit state on CPU {cpu}"
            ));
        }
        if rollback.rejected_cpu.is_some() {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: local IRQ lifecycle rollback was rejected"
            ));
        }
    }
    Err(Error::LocalInterruptCrossCallFailed(rejected))
}

pub(super) enum LocalLifecycleApply {
    Applied,
    Rejected,
    AppliedOrUnknown,
}

pub(super) fn apply_local_irq_lifecycle(
    hardware: InterruptId,
    priority: InterruptPriority,
    trigger: InterruptTrigger,
    operation: LocalLifecycleOperation,
) -> LocalLifecycleApply {
    let Some(cpu) = crate::cpu::current_index() else {
        return LocalLifecycleApply::Rejected;
    };
    let Some(controller) = LOCAL_CONTROLLERS[cpu].get() else {
        return LocalLifecycleApply::Rejected;
    };
    match operation {
        // Configuration runs while the route is disabled. A partial priority
        // or trigger write can be overwritten by a later prepare attempt and
        // therefore remains a recoverable rejection.
        LocalLifecycleOperation::Configure => {
            if controller.configure(hardware, priority, trigger).is_ok() {
                LocalLifecycleApply::Applied
            } else {
                LocalLifecycleApply::Rejected
            }
        }
        LocalLifecycleOperation::Enable => classify_local_transition(controller.enable(hardware)),
        LocalLifecycleOperation::Disable => classify_local_transition(controller.disable(hardware)),
    }
}

fn classify_local_transition(
    result: Result<(), InterruptTransitionError<crate::hal::irq::ControllerError>>,
) -> LocalLifecycleApply {
    match result {
        Ok(()) => LocalLifecycleApply::Applied,
        Err(InterruptTransitionError::NotApplied(_)) => LocalLifecycleApply::Rejected,
        Err(InterruptTransitionError::AppliedOrUnknown(_)) => LocalLifecycleApply::AppliedOrUnknown,
    }
}

fn require_local_lifecycle_available(
    controller: &BootInterruptController,
    interrupt: InterruptId,
) -> Result<(), Error> {
    if controller.is_per_cpu(interrupt) && crate::cpu::online_cpu_count() > 1 {
        Err(Error::LocalInterruptLifecycleRequiresCrossCall)
    } else {
        Ok(())
    }
}
