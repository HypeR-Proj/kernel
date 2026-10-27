// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VM construction, boot-vCPU preparation, and atomic publication typestates.

use hyper::mm::FallibleArc;

use super::execution::{VirtualMachine, VmBinding};
use super::resources::VmLifecycleResources;
use super::{Error, REGISTRY, VmId, VmReservation};
use crate::accounting::CommittedCharge;
use crate::task::thread::ThreadId;
use crate::vm::VmInterruptController;
use crate::vm::device::VirtualDeviceSet;
use crate::vm::memory::GuestAddressSpace;

/// Locally complete VM state awaiting its single registry publication.
pub(crate) struct VmBuilder {
    machine: FallibleArc<VirtualMachine>,
    // Drop last so the logical and hardware identities cannot be reused while
    // unpublished VM-owned resources are still being destroyed.
    reservation: VmReservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VcpuPreparationError {
    Registry(Error),
    Scheduler(crate::task::scheduler::Error),
}

impl VmBuilder {
    pub(crate) fn new(
        reservation: VmReservation,
        lifecycle_resources: VmLifecycleResources,
        configuration: crate::vm::installed::VirtualMachineConfiguration,
        address_space: GuestAddressSpace,
        interrupts: VmInterruptController,
        interrupt_controller_charge: Option<CommittedCharge>,
        devices: VirtualDeviceSet,
    ) -> Result<Self, Error> {
        let id = reservation.id;
        let lifecycle =
            crate::vm::installed::InstalledMachine::try_new(configuration, lifecycle_resources)
                .map_err(|_| Error::Allocation)?;
        let machine = VirtualMachine::try_new(
            id,
            address_space,
            interrupts,
            devices,
            lifecycle,
            interrupt_controller_charge,
        )?;
        Ok(Self {
            machine,
            reservation,
        })
    }

    fn vcpu_binding(&self) -> VmBinding {
        VmBinding::new(self.reservation.id, self.machine.clone())
    }

    /// Prepares the non-runnable scheduler object and absorbs its rollback
    /// capability into the installation transaction.
    ///
    /// Only the returned typestate exposes installation. Neither the VM
    /// binding nor the reserved `ThreadId` can escape through a safe API.
    pub(crate) fn prepare_boot_vcpu(
        self,
        vcpu_id: u32,
        bootstrap: crate::vm::objects::VirtualCpuBootstrap,
    ) -> Result<PreparedVm, VcpuPreparationError> {
        let count = self.machine.lifecycle().configuration().vcpu_count;
        let mut dormant = alloc::vec::Vec::new();
        dormant
            .try_reserve_exact(count as usize)
            .map_err(|_| VcpuPreparationError::Registry(Error::Allocation))?;
        let mut boot_vcpu = None;
        for id in 0..count {
            let context = (id == vcpu_id).then_some(bootstrap);
            let resources = self
                .machine
                .lifecycle()
                .reserve_vcpu_runtime()
                .map_err(VcpuPreparationError::Registry)?;
            let prepared =
                crate::vm::vcpu::create_thread(self.vcpu_binding(), id, context, resources)
                    .map_err(VcpuPreparationError::Scheduler)?;
            // SAFETY: The aggregate owns all rollback guards until publication.
            let thread = unsafe { prepared.id_for_vm_install() };
            let endpoint = self
                .machine
                .endpoint(id)
                .map_err(VcpuPreparationError::Registry)?;
            if endpoint.bind_thread(thread).is_err() {
                hyper::debug::invariant_failure(
                    "vm::registry::construction::prepare_boot_vcpu invariant",
                );
            }
            if id == vcpu_id {
                boot_vcpu = Some(thread);
            }
            dormant.push(prepared);
        }
        let thread = boot_vcpu.ok_or(VcpuPreparationError::Registry(Error::UnknownVcpu))?;
        Ok(PreparedVm {
            dormant,
            machine: self.machine,
            reservation: self.reservation,
            boot_vcpu: thread,
        })
    }
}

/// Fully allocated VM aggregate that can only be installed or rolled back.
pub(crate) struct PreparedVm {
    // Drop first so an unpublished vCPU releases its strong VM binding before
    // the builder owner and identity are released.
    dormant: alloc::vec::Vec<crate::task::scheduler::DormantVcpuThread>,
    machine: FallibleArc<VirtualMachine>,
    // Drop last for the same identity-reuse ordering as VmBuilder.
    reservation: VmReservation,
    boot_vcpu: ThreadId,
}

impl PreparedVm {
    pub(in crate::vm) fn lifecycle(&self) -> FallibleArc<crate::vm::installed::InstalledMachine> {
        self.machine.lifecycle()
    }

    pub(crate) fn set_physical(&self, physical: crate::device::assigned::AssignmentSet) {
        self.machine.set_physical(physical);
    }

    /// Completes every fallible prerequisite for registry publication.
    pub(crate) fn install(self) -> Result<InstalledVm, Error> {
        let id = self.reservation.id;
        REGISTRY.with(|registry| registry.validate_install(id, &self.machine))?;
        let physical = self.machine.physical_owners();
        self.machine.prepare_dma()?;
        // Activation can fail after an earlier controller is active. The same
        // rollback covers that partial activation and later VMID failure.
        let activated = physical
            .activate_for(id)
            .map_err(|_| Error::Allocation)
            .and_then(|()| self.machine.activate_identifier_for_install());
        if let Err(error) = activated {
            if physical.quiesce_all().is_err() || self.machine.retire_dma().is_err() {
                let _retained = core::mem::ManuallyDrop::new(self);
                crate::diagnostics::crash::fatal(format_args!(
                    "physical assignments rollback could not quiesce"
                ));
            }
            return Err(error);
        }
        self.machine.bind_virtual_serial(0);
        let lifecycle = self.machine.lifecycle();
        let Self {
            dormant,
            machine,
            mut reservation,
            boot_vcpu: _boot_vcpu,
        } = self;

        // Preserve PreparedVm's declared rollback drop order through every
        // fallible operation. This infallible publication tail may now split
        // its fields into independent local owners.
        let mut machine = Some(machine);
        // The first locked validation proved this uniquely reserved slot and
        // complete machine. A live reservation prevents any intervening slot
        // transition, so final publication is infallible after VMID activation.
        REGISTRY.with(|registry| registry.install_prevalidated(id, &mut machine));
        reservation.unpublished = false;
        drop(reservation);
        for thread in dormant {
            // SAFETY: Installation transferred one strong owner into the registry
            // before exposing the ThreadId retained by the VM and vCPU binding.
            let _ = unsafe { thread.commit_after_vm_install() };
        }

        Ok(InstalledVm {
            id,
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            boot_vcpu: _boot_vcpu,
            control: VmControl::mint_for_install(id),
            lifecycle,
        })
    }
}

/// Sole authority to start retirement of one installed VM incarnation.
///
/// Only `PreparedVm::install` can mint this non-Clone token. In particular,
/// no module can reconstruct it from the intentionally Copy `VmId`.
#[must_use = "the installed VM lifecycle authority must be retired explicitly"]
pub(in crate::vm) struct VmControl {
    id: VmId,
}

impl VmControl {
    const fn mint_for_install(id: VmId) -> Self {
        Self { id }
    }

    pub(super) const fn id(&self) -> VmId {
        self.id
    }
}

/// Capabilities exposed only after complete VM publication.
#[must_use = "bind the installed VM identity, vCPU, and lifecycle authority"]
pub(crate) struct InstalledVm {
    id: VmId,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    boot_vcpu: ThreadId,
    control: VmControl,
    lifecycle: FallibleArc<crate::vm::installed::InstalledMachine>,
}

impl InstalledVm {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) const fn id_for_test(&self) -> VmId {
        self.id
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(in crate::vm) fn into_boot_parts(self) -> (VmId, ThreadId, VmControl) {
        (self.id, self.boot_vcpu, self.control)
    }

    pub(in crate::vm) fn publish_handle_lifecycle(
        self,
    ) -> FallibleArc<crate::vm::installed::InstalledMachine> {
        self.lifecycle.publish_installed(self.id, self.control);
        self.lifecycle
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[allow(
        dead_code,
        reason = "the executable migration fixture has only AArch64 and RISC-V guest payloads"
    )]
    pub(crate) fn start_boot_for_test(self) -> Result<RunningVmForTest, &'static str> {
        let running = RunningVmForTest {
            thread: self.boot_vcpu,
            owner: self.publish_handle_lifecycle(),
        };
        if running.owner.start_vcpu(0).is_err() {
            running.stop();
            return Err("installed boot vCPU did not start");
        }
        Ok(running)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) const fn boot_vcpu_for_test(&self) -> ThreadId {
        self.boot_vcpu
    }
}

/// Retains test RAM until the test explicitly requests administrative stop.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[allow(
    dead_code,
    reason = "the executable migration fixture has only AArch64 and RISC-V guest payloads"
)]
pub(crate) struct RunningVmForTest {
    thread: ThreadId,
    owner: FallibleArc<crate::vm::installed::InstalledMachine>,
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl RunningVmForTest {
    #[allow(
        dead_code,
        reason = "the executable migration fixture has only AArch64 and RISC-V guest payloads"
    )]
    pub(crate) const fn thread(&self) -> ThreadId {
        self.thread
    }

    #[allow(
        dead_code,
        reason = "the executable migration fixture has only AArch64 and RISC-V guest payloads"
    )]
    pub(crate) fn stop(self) {
        crate::vm::installed::InstalledMachine::request_stop(&self.owner);
    }
}
