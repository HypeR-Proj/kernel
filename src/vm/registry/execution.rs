// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Installed VM aggregate, strong bindings, and execution claims.

use hyper::cpu::CpuIndex;
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {
    hyper_vm_models::translation::ConcurrentExecution,
    hyper_vm_models::translation::ExecutionClaim, hyper_vm_models::translation::ExecutionError,
};

use super::{Error, VmId};
use crate::accounting::CommittedCharge;
use crate::task::thread::ThreadId;
use crate::vm::VmInterruptController;
use crate::vm::device::VirtualDeviceSet;
use crate::vm::memory::GuestAddressSpace;

type AddressSpaceLock = InterruptSpinLock<GuestAddressSpace, crate::hal::irq::LocalMask>;

/// Non-cloneable vCPU capability retaining strong ownership of its VM.
pub(crate) struct VmBinding {
    id: VmId,
    machine: FallibleArc<VirtualMachine>,
}

impl VmBinding {
    pub(in crate::vm) fn dma(
        &self,
    ) -> Option<FallibleArc<crate::device::iommu::address_space::AddressSpace>> {
        self.machine.dma.with(|dma| dma.clone())
    }
    pub(super) fn new(id: VmId, machine: FallibleArc<VirtualMachine>) -> Self {
        Self { id, machine }
    }

    pub(crate) fn lifecycle(&self) -> FallibleArc<crate::vm::installed::InstalledMachine> {
        self.machine.lifecycle()
    }

    pub(crate) fn publish_changed_interrupts(&self) {
        let mut targets = crate::hal::vm::take_reconcile_targets(self.interrupts());
        while targets != 0 {
            let cpu = targets.trailing_zeros();
            targets &= targets - 1;
            if let Ok(endpoint) = self.endpoint(cpu)
                && let Some(thread) = endpoint.thread()
            {
                match self.publish_interrupt_reconcile(cpu, thread) {
                    Ok(()) | Err(Error::EndpointClosed) => {}
                    Err(error) => crate::diagnostics::crash::fatal(format_args!(
                        "guest interrupt notification failed: {error:?}"
                    )),
                }
            }
        }
    }

    pub(crate) const fn id(&self) -> VmId {
        self.id
    }

    /// Admits at most four detailed terminal-MMIO reports and one final
    /// suppression notice over this installed VM's complete lifetime.
    #[allow(dead_code, reason = "x86 has no terminal MMIO-report producer")]
    pub(in crate::vm) fn admit_unhandled_mmio(
        &self,
        vcpu: u32,
        access: hyper_vm_models::exit::MmioAccess,
    ) -> Option<crate::vm::diagnostics::UnhandledMmioReport> {
        self.machine
            .diagnostics
            .admit_unhandled_mmio(self.id.diagnostic_id(), vcpu, access)
    }

    #[allow(
        dead_code,
        reason = "selected guest platforms enter these routes only when their HAL supports assignment or notifications"
    )]
    pub(crate) fn route_physical_mmio(
        &self,
        access: hyper_vm_models::exit::MmioAccess,
    ) -> Option<hyper_vm_models::exit::MmioAction> {
        let (object, offset) = self
            .machine
            .physical
            .with(|physical| physical.mmio_owner(access))?;
        Some(object.object().access_at(offset, access))
    }
    #[allow(
        dead_code,
        reason = "selected guest platforms enter these routes only when their HAL supports assignment or notifications"
    )]
    pub(crate) fn route_io_mmio(
        &self,
        access: hyper_vm_models::exit::MmioAccess,
    ) -> Option<hyper_vm_models::exit::MmioAction> {
        self.machine.io_routes.route(access)
    }
    pub(crate) fn preflight_io_route(
        &self,
        route: &crate::vm::io::Route,
    ) -> Result<(), crate::vm::io::Error> {
        if self
            .machine
            .physical
            .with(|physical| physical.has_interrupt(route.irq()))
        {
            return Err(crate::vm::io::Error::Busy);
        }
        self.machine.io_routes.can_insert(route)
    }
    pub(crate) fn owns_userspace_assignment_aperture(&self, base: u64, length: u64) -> bool {
        self.machine
            .physical
            .with(|physical| physical.owns_userspace_aperture(base, length))
    }
    pub(crate) fn access_notification(
        &self,
        original: u64,
        operation: u32,
        value: u32,
    ) -> Result<u32, crate::vm::io::Error> {
        self.machine
            .io_routes
            .access_frontend(original, operation, value)
    }

    pub(crate) fn configure_notification(
        &self,
        original: u64,
        base: u64,
        flags: u32,
    ) -> Result<(), crate::vm::io::Error> {
        self.machine
            .io_routes
            .configure_frontend(original, base, flags)
    }
    pub(crate) fn io_range_conflicts(&self, base: u64, length: u64) -> bool {
        self.machine.io_routes.conflicts(base, length)
    }
    pub(crate) fn install_io_route(
        &self,
        route: crate::vm::io::Route,
    ) -> Result<(), crate::vm::io::Error> {
        self.machine.io_routes.insert(route)
    }

    pub(crate) fn remove_io_notification(&self, route_id: u64) -> Option<crate::vm::io::Route> {
        self.machine.io_routes.remove_notification(route_id)
    }

    pub(crate) fn interrupts(&self) -> &VmInterruptController {
        &self.machine.interrupts
    }

    fn endpoint(&self, vcpu: u32) -> Result<&crate::vm::endpoint::VcpuEndpoint, Error> {
        self.machine.endpoint(vcpu)
    }

    pub(in crate::vm) fn endpoint_owner(
        &self,
        vcpu: u32,
    ) -> Result<FallibleArc<crate::vm::endpoint::VcpuEndpoint>, Error> {
        self.machine
            .lifecycle
            .endpoint(vcpu)
            .cloned()
            .map_err(|_| Error::UnknownVcpu)
    }

    pub(crate) fn claim_detached_interrupt_reconcile(&self, vcpu: u32) -> Result<bool, Error> {
        self.endpoint(vcpu)
            .map(|endpoint| endpoint.claim_detached_reconcile())
    }

    pub(crate) fn restore_interrupt_reconcile(&self, vcpu: u32) -> Result<(), Error> {
        self.endpoint(vcpu)?.restore_reconcile();
        Ok(())
    }

    pub(crate) fn interrupt_reconcile_pending(&self, vcpu: u32) -> Result<bool, Error> {
        self.endpoint(vcpu)
            .map(crate::vm::endpoint::VcpuEndpoint::reconcile_pending)
    }

    /// Publishes a completed saved interrupt-model mutation, then prompts the
    /// scheduler-authoritative running CPU, if any.
    pub(in crate::vm) fn publish_interrupt_reconcile(
        &self,
        vcpu: u32,
        expected_thread: ThreadId,
    ) -> Result<(), Error> {
        let endpoint = self.endpoint(vcpu)?;
        if endpoint.thread() != Some(expected_thread) {
            return Err(Error::StaleIdentity);
        }
        match endpoint.publish_reconcile() {
            Ok(()) => {}
            Err(crate::vm::endpoint_state::StateError::Closed(_)) => {
                return Err(Error::EndpointClosed);
            }
            Err(crate::vm::endpoint_state::StateError::Corrupt) => {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: vCPU endpoint contains an invalid lifecycle state"
                ));
            }
        }
        endpoint.signal_waiter();
        let Some(cpu) = crate::task::scheduler::running_vcpu_cpu(expected_thread)
            .map_err(|_| Error::Scheduler)?
        else {
            return Ok(());
        };
        // A local sender may have drained a remote producer's controller bit
        // after its last refill. Synchronous guest return has no IRQ-tail
        // checkpoint, so local targets also need a hardware prompt.
        if crate::hal::vm::request_guest_exit(cpu) {
            return Ok(());
        }
        // A failed hardware route may be stale because migration happened
        // after the first immutable scheduler observation. Retry one changed
        // exact target; a still-Running unpromptable Thread cannot safely be
        // allowed to continue with an unreflected interrupt model.
        let target = crate::task::scheduler::running_vcpu_cpu(expected_thread)
            .map_err(|_| Error::Scheduler)?;
        match target {
            None => {}
            Some(target) if target != cpu && crate::hal::vm::request_guest_exit(target) => {}
            Some(_) => crate::diagnostics::crash::fatal(format_args!(
                "HypeR: scheduler reports a running vCPU without a qualified guest-exit route"
            )),
        }
        Ok(())
    }

    pub(in crate::vm) fn wfi_wait_ticket(
        &self,
        vcpu: u32,
    ) -> Result<crate::vm::endpoint::WaitTicket, Error> {
        Ok(self.endpoint(vcpu)?.wait_ticket())
    }

    pub(in crate::vm) fn prepare_wfi_wait(
        &self,
        vcpu: u32,
        ticket: crate::vm::endpoint::WaitTicket,
    ) -> Result<crate::vm::endpoint::PreparedWait<'_>, Error> {
        self.endpoint(vcpu)?
            .prepare_wait(ticket)
            .map_err(|_| Error::Scheduler)
    }

    pub(in crate::vm) fn arm_wfi_timer(
        &self,
        vcpu: u32,
        deadline: u64,
    ) -> Result<crate::time::ArmedIrqCallout<'_>, Error> {
        self.endpoint(vcpu)?
            .arm_timer(deadline)
            .map_err(|_| Error::Scheduler)
    }

    pub(in crate::vm) fn close_vcpu_endpoint(
        &self,
        vcpu: u32,
        expected_thread: ThreadId,
        reason: crate::hal::vm::VcpuTerminalReason,
    ) -> Result<crate::vm::endpoint_state::GuestCloseOutcome, Error> {
        let reason = match reason {
            crate::hal::vm::VcpuTerminalReason::MemoryFault => {
                crate::vm::endpoint_state::TerminalReason::MemoryFault
            }
            crate::hal::vm::VcpuTerminalReason::Mmio => {
                crate::vm::endpoint_state::TerminalReason::Mmio
            }
            crate::hal::vm::VcpuTerminalReason::Synchronous => {
                crate::vm::endpoint_state::TerminalReason::Synchronous
            }
        };
        self.endpoint(vcpu)?
            .close(expected_thread, reason)
            .map_err(|_| Error::StaleIdentity)
    }

    pub(in crate::vm) fn administrative_stop_requested(
        &self,
        vcpu: u32,
        expected_thread: ThreadId,
    ) -> Result<Option<crate::vm::endpoint_state::AdministrativeStopReason>, Error> {
        self.endpoint(vcpu)?
            .stop_requested(expected_thread)
            .map_err(|_| Error::StaleIdentity)
    }

    pub(in crate::vm) fn publish_hardware_detached(
        &self,
        vcpu: u32,
        expected_thread: ThreadId,
        reason: crate::vm::endpoint_state::AdministrativeStopReason,
    ) -> Result<(), Error> {
        self.endpoint(vcpu)?
            .publish_hardware_detached(expected_thread, reason)
            .map_err(|_| Error::StaleIdentity)
    }

    pub(in crate::vm) fn devices(&self) -> &VirtualDeviceSet {
        &self.machine.devices
    }

    pub(in crate::vm) fn with_address_space<R>(
        &self,
        operation: impl FnOnce(&mut GuestAddressSpace) -> R,
    ) -> R {
        self.machine.address_space.with(operation)
    }

    /// Admits concurrent vCPU execution while retaining per-CPU hardware ownership.
    /// The scheduler exclusively owns each vCPU's execution payload.
    pub(crate) fn claim_execution(
        &self,
        cpu: CpuIndex,
    ) -> Result<VmExecutionClaim, VmExecutionError> {
        let admission = self
            .machine
            .run_admission
            .admit()
            .map_err(|error| match error {
                crate::vm::run_admission::AdmissionError::Closed => {
                    VmExecutionError::AdmissionClosed
                }
                crate::vm::run_admission::AdmissionError::CountExhausted => {
                    VmExecutionError::AdmissionCountExhausted
                }
            })?;
        match self.machine.execution.claim(cpu) {
            Ok(execution) => Ok(VmExecutionClaim {
                execution,
                admission,
                residency: None,
            }),
            Err(error) => {
                self.machine.run_admission.release(admission);
                Err(VmExecutionError::Execution(error))
            }
        }
    }

    pub(crate) fn release_execution(
        &self,
        claim: VmExecutionClaim,
        current_cpu: CpuIndex,
    ) -> Result<(), VmExecutionReleaseFailure> {
        if claim.residency.is_some() {
            // Architecture detach must consume guest residency before the
            // execution/admission capability can cross this release boundary.
            hyper::debug::invariant_failure("vm::registry::execution::release_execution invariant")
        }
        let VmExecutionClaim {
            execution,
            admission,
            residency: _,
        } = claim;
        match self.machine.execution.release(execution, current_cpu) {
            Ok(()) => {
                // Admission release cannot fail through this private API. It
                // follows successful execution release, so every returned
                // failure below retains both exact armed capabilities.
                self.machine.run_admission.release(admission);
                Ok(())
            }
            Err(failure) => Err(VmExecutionReleaseFailure {
                error: failure.error(),
                claim: VmExecutionClaim {
                    execution: failure.into_claim(),
                    admission,
                    residency: None,
                },
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmExecutionError {
    AdmissionClosed,
    AdmissionCountExhausted,
    Execution(ExecutionError),
}

#[must_use = "VM execution ownership must remain live until release succeeds"]
pub(crate) struct VmExecutionClaim {
    execution: ExecutionClaim,
    admission: crate::vm::run_admission::RunAdmissionClaim,
    residency: Option<crate::vm::memory::GuestResidencyClaim>,
}

impl VmExecutionClaim {
    pub(crate) fn attach_residency(
        &mut self,
        residency: crate::vm::memory::GuestResidencyClaim,
    ) -> Result<(), crate::vm::memory::GuestResidencyClaim> {
        if self.residency.is_some() {
            return Err(residency);
        }
        self.residency = Some(residency);
        Ok(())
    }

    pub(crate) fn take_residency(&mut self) -> Option<crate::vm::memory::GuestResidencyClaim> {
        self.residency.take()
    }

    pub(crate) fn restore_residency(&mut self, residency: crate::vm::memory::GuestResidencyClaim) {
        if self.residency.replace(residency).is_some() {
            hyper::debug::invariant_failure("vm::registry::execution::restore_residency invariant")
        }
    }
}

#[must_use = "a failed VM execution release retains both exact claims"]
pub(crate) struct VmExecutionReleaseFailure {
    error: ExecutionError,
    claim: VmExecutionClaim,
}

impl VmExecutionReleaseFailure {
    pub(crate) const fn error(&self) -> ExecutionError {
        self.error
    }

    pub(crate) fn into_claim(self) -> VmExecutionClaim {
        self.claim
    }
}

pub(super) struct VirtualMachine {
    id: VmId,
    address_space: AddressSpaceLock,
    dma: InterruptSpinLock<
        Option<FallibleArc<crate::device::iommu::address_space::AddressSpace>>,
        crate::hal::irq::LocalMask,
    >,
    execution: ConcurrentExecution,
    run_admission: crate::vm::run_admission::RunAdmission,
    interrupts: VmInterruptController,
    devices: VirtualDeviceSet,
    physical: InterruptSpinLock<crate::device::assigned::AssignmentSet, crate::hal::irq::LocalMask>,
    io_routes: crate::vm::io::Routes,
    diagnostics: crate::vm::diagnostics::VmDiagnostics,
    lifecycle: FallibleArc<crate::vm::installed::InstalledMachine>,
    _interrupt_controller_charge: Option<CommittedCharge>,
}

impl VirtualMachine {
    pub(super) fn dma_owner(
        &self,
    ) -> Option<FallibleArc<crate::device::iommu::address_space::AddressSpace>> {
        self.dma.with(|slot| slot.clone())
    }
    pub(super) fn prepare_dma(&self) -> Result<(), Error> {
        let streams = self.physical_owners().dma_streams();
        if streams.iter().all(Option::is_none) {
            return Ok(());
        }
        // Register durable observers before any DMA domain or device is activated.
        self.physical_owners()
            .observe_dma_faults()
            .map_err(|_| Error::Allocation)?;
        let (memory, domain) = self.address_space.with(|space| space.dma_snapshot());
        let leases = memory.dma_leases(&domain).map_err(|_| Error::Allocation)?;
        let dma =
            crate::device::iommu::address_space::AddressSpace::create(streams, &leases, domain)
                .map_err(|_| Error::Allocation)?;
        #[cfg(all(
            CONFIG_ARCH_AARCH64,
            all(feature = "kernel-test", feature = "io-dma-revoke-test")
        ))]
        if streams.contains(&Some(16)) {
            crate::device::iommu::revoke_test::start(dma.clone());
        }
        self.dma.with(|slot| *slot = Some(dma));
        crate::pr_info!("HypeR: VM {:?}: physical PCI DMA domain installed", self.id);
        Ok(())
    }

    pub(super) fn retire_dma(&self) -> Result<(), Error> {
        if let Some(dma) = self.dma.with(|dma| dma.clone()) {
            dma.retire().map_err(|_| Error::Allocation)?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn try_new(
        id: VmId,
        address_space: GuestAddressSpace,
        interrupts: VmInterruptController,
        devices: VirtualDeviceSet,
        lifecycle: FallibleArc<crate::vm::installed::InstalledMachine>,
        interrupt_controller_charge: Option<CommittedCharge>,
    ) -> Result<FallibleArc<Self>, Error> {
        FallibleArc::try_new(Self {
            id,
            address_space: InterruptSpinLock::new(address_space),
            dma: InterruptSpinLock::new(None),
            execution: ConcurrentExecution::new(id.execution_owner()),
            run_admission: crate::vm::run_admission::RunAdmission::new(id.execution_owner()),
            interrupts,
            devices,
            physical: InterruptSpinLock::new(crate::device::assigned::AssignmentSet::new()),
            io_routes: crate::vm::io::Routes::new(),
            diagnostics: crate::vm::diagnostics::VmDiagnostics::new(),
            lifecycle,
            _interrupt_controller_charge: interrupt_controller_charge,
        })
        .map_err(|_| Error::Allocation)
    }

    pub(super) const fn id(&self) -> VmId {
        self.id
    }

    pub(super) fn allocation_size() -> usize {
        FallibleArc::<Self>::allocation_size()
    }

    pub(super) fn lifecycle(&self) -> FallibleArc<crate::vm::installed::InstalledMachine> {
        self.lifecycle.clone()
    }

    pub(super) fn activate_identifier_for_install(&self) -> Result<(), Error> {
        self.address_space
            .with(GuestAddressSpace::activate_identifier_for_install)
    }

    pub(super) fn with_address_space<R>(
        &self,
        operation: impl FnOnce(&mut GuestAddressSpace) -> R,
    ) -> R {
        self.address_space.with(operation)
    }

    pub(super) fn endpoint(&self, vcpu: u32) -> Result<&crate::vm::endpoint::VcpuEndpoint, Error> {
        self.lifecycle
            .endpoint(vcpu)
            .map(|endpoint| &**endpoint)
            .map_err(|_| Error::UnknownVcpu)
    }

    pub(super) fn request_all_stops(&self) -> Result<(), Error> {
        for endpoint in self.lifecycle.endpoints() {
            let Some(thread) = endpoint.thread() else {
                if endpoint.lifecycle().map_err(|_| Error::StaleIdentity)?
                    != crate::vm::endpoint_state::Lifecycle::Unbound
                {
                    return Err(Error::StaleIdentity);
                }
                continue;
            };
            match endpoint
                .request_stop(
                    thread,
                    crate::vm::endpoint_state::AdministrativeStopReason::Requested,
                )
                .map_err(|_| Error::StaleIdentity)?
            {
                crate::vm::endpoint_state::StopRequestOutcome::Published
                | crate::vm::endpoint_state::StopRequestOutcome::AlreadyRequested => {
                    match crate::task::scheduler::request_vcpu_stop(thread) {
                        Ok(()) => {}
                        Err(crate::task::scheduler::Error::ThreadNotFound) => {
                            if !endpoint
                                .thread_absence_is_terminal()
                                .map_err(|_| Error::StaleIdentity)?
                            {
                                return Err(Error::Scheduler);
                            }
                        }
                        Err(_) => return Err(Error::Scheduler),
                    }
                }
                crate::vm::endpoint_state::StopRequestOutcome::GuestTerminal(_)
                | crate::vm::endpoint_state::StopRequestOutcome::HardwareDetached
                | crate::vm::endpoint_state::StopRequestOutcome::Reaped
                | crate::vm::endpoint_state::StopRequestOutcome::Inactive => {}
            }
        }
        self.run_admission.close();
        Ok(())
    }

    pub(super) fn bind_virtual_serial(&self, vcpu: u32) {
        self.devices.bind_virtual_serial(self.id, vcpu);
    }

    pub(super) fn disconnect_virtual_serial(&self) {
        self.devices.disconnect_virtual_serial(self.id);
    }

    pub(super) fn quiesce_devices(&self) -> Result<(), super::super::device::Error> {
        super::super::device::quiesce(&self.devices)
    }

    pub(super) fn set_physical(&self, physical: crate::device::assigned::AssignmentSet) {
        let old = self
            .physical
            .with(|slot| core::mem::replace(slot, physical));
        drop(old);
    }
    pub(super) fn physical_owners(&self) -> crate::device::assigned::AssignmentOwners {
        self.physical.with(|physical| physical.owners())
    }
    pub(super) fn close_io_routes(&self) {
        self.io_routes.close();
    }

    pub(super) fn is_quiescent(&self) -> bool {
        self.run_admission.is_closed_and_quiescent()
            && self.lifecycle.endpoints().iter().all(|endpoint| {
                matches!(
                    endpoint.lifecycle(),
                    Ok(crate::vm::endpoint_state::Lifecycle::Unbound
                        | crate::vm::endpoint_state::Lifecycle::Reaped(_))
                )
            })
    }
}
