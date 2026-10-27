// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-owned execution state for one virtual CPU.
//!
//! This payload is VM policy, not generic Thread state. The scheduler owns its
//! allocation and stable address, while this module owns every interpretation
//! of the guest binding, hardware registers, and terminal publication.

use crate::task::thread::ThreadId;

pub(crate) struct VcpuExecution {
    vm: VcpuVm,
    instruction_context: hyper_vm_models::translation::GuestInstructionContext,
    terminal_mmio_report: Option<crate::vm::UnhandledMmioReport>,
    reap: ReapOwnership,
    pub(crate) vcpu_id: u32,
    pub(crate) hardware: crate::hal::vm::VcpuHardwareState,
}

// Migration eligibility remains compiler-proven. CPU-affine hardware and
// residency claims live in `vm::active_vcpu`, never in this movable payload.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<VcpuExecution>();
};

enum VcpuVm {
    Installed(crate::vm::registry::VmBinding),
    TimerValidation {
        interrupts: alloc::boxed::Box<crate::vm::VmInterruptController>,
    },
}

enum ReapOwnership {
    Installed(alloc::boxed::Box<VcpuReapPublication>),
    None,
}

impl VcpuExecution {
    pub(crate) const fn allocation_size() -> Option<usize> {
        Some(core::mem::size_of::<Self>())
    }

    pub(crate) const fn retirement_allocation_size() -> usize {
        core::mem::size_of::<VcpuReapPublication>()
    }

    // Keep architectural register construction in its own measured frame;
    // callers retain only bootstrap metadata and the final heap owner.
    #[inline(never)]
    pub(crate) fn try_installed(
        vm: crate::vm::registry::VmBinding,
        vcpu_id: u32,
        bootstrap: Option<crate::vm::objects::VirtualCpuBootstrap>,
        entry_ready: &crate::hal::vm::VmEntryReady,
    ) -> Result<alloc::boxed::Box<core::cell::UnsafeCell<Self>>, crate::task::thread::Error> {
        // Allocate before constructing architectural state. Neither the VM
        // preparation loop nor its syscall caller carries a full register bank.
        let mut storage = hyper::mm::try_box_uninit::<core::cell::UnsafeCell<Self>>()
            .map_err(|_| crate::task::thread::Error::Allocation)?;
        let endpoint = vm
            .endpoint_owner(vcpu_id)
            .map_err(|_| crate::task::thread::Error::InvalidPlacement)?;
        let reap = hyper::mm::try_box(VcpuReapPublication::new(endpoint, vcpu_id, vm.lifecycle()))
            .map_err(|_| crate::task::thread::Error::Allocation)?;
        let mut context = match bootstrap {
            Some(bootstrap) => crate::hal::vm::prepare_native_bootstrap_context(
                bootstrap.entry,
                bootstrap.stack,
                bootstrap.arguments,
            )
            .map_err(|_| crate::task::thread::Error::InvalidPlacement)?,
            None => crate::hal::vm::VcpuContext::new(0),
        };
        crate::hal::vm::set_virtual_count(&mut context, 0, 0);
        let target = storage.as_mut_ptr().cast::<Self>();
        // SAFETY: The unique allocation is aligned for UnsafeCell<Self>, whose
        // representation is transparent. Each field is written exactly once;
        // no fallible operation occurs until all fields have owners. No full
        // VcpuExecution temporary is constructed on the kernel Thread stack.
        unsafe {
            core::ptr::addr_of_mut!((*target).vm).write(VcpuVm::Installed(vm));
            core::ptr::addr_of_mut!((*target).instruction_context)
                .write(hyper_vm_models::translation::GuestInstructionContext::new());
            core::ptr::addr_of_mut!((*target).terminal_mmio_report).write(None);
            core::ptr::addr_of_mut!((*target).reap).write(ReapOwnership::Installed(reap));
            core::ptr::addr_of_mut!((*target).vcpu_id).write(vcpu_id);
            core::ptr::addr_of_mut!((*target).hardware)
                .write(crate::hal::vm::VcpuHardwareState::new(context, entry_ready));
        }
        // SAFETY: Every field was initialized above; ownership now guarantees
        // ordinary cleanup if interrupt initialization fails.
        let mut execution = unsafe { storage.assume_init() };
        crate::hal::vm::initialize_vcpu_interrupts(&mut execution.get_mut().hardware)?;
        Ok(execution)
    }

    pub(crate) fn vm_binding(&self) -> Option<&crate::vm::registry::VmBinding> {
        match &self.vm {
            VcpuVm::Installed(binding) => Some(binding),
            VcpuVm::TimerValidation { .. } => None,
        }
    }

    pub(super) fn enter_instruction_context(
        &mut self,
        cpu: hyper::cpu::CpuIndex,
    ) -> hyper_vm_models::translation::GuestInstructionContextTransition {
        self.instruction_context.enter(cpu)
    }

    #[allow(
        dead_code,
        reason = "x86 guest exits currently use port I/O rather than the MMIO device path"
    )]
    pub(crate) fn device_context(
        &mut self,
    ) -> Option<(
        &crate::vm::registry::VmBinding,
        &mut crate::hal::vm::VcpuHardwareState,
        u32,
        &crate::vm::VmInterruptController,
    )> {
        match &self.vm {
            VcpuVm::Installed(binding) => Some((
                binding,
                &mut self.hardware,
                self.vcpu_id,
                binding.interrupts(),
            )),
            VcpuVm::TimerValidation { .. } => None,
        }
    }

    /// Splits the mutable hardware state from its immutable interrupt model.
    ///
    /// Keeping this split inside the owning type lets Rust prove that callers
    /// never receive an immutable reference into a value which they may also
    /// replace wholesale through `&mut VcpuExecution`.
    pub(crate) fn interrupt_context(
        &mut self,
    ) -> (
        &mut crate::hal::vm::VcpuHardwareState,
        u32,
        &crate::vm::VmInterruptController,
    ) {
        let interrupts = match &self.vm {
            VcpuVm::Installed(binding) => binding.interrupts(),
            VcpuVm::TimerValidation { interrupts } => interrupts,
        };
        (&mut self.hardware, self.vcpu_id, interrupts)
    }

    pub(crate) fn interrupts(&self) -> &crate::vm::VmInterruptController {
        match &self.vm {
            VcpuVm::Installed(binding) => binding.interrupts(),
            VcpuVm::TimerValidation { interrupts } => interrupts,
        }
    }

    #[allow(
        dead_code,
        reason = "x86 guest exits currently use port I/O rather than the MMIO device path"
    )]
    pub(crate) fn publish_terminal_mmio_report(
        &mut self,
        report: crate::vm::UnhandledMmioReport,
    ) -> Result<(), ()> {
        if self.terminal_mmio_report.is_some() {
            return Err(());
        }
        self.terminal_mmio_report = Some(report);
        Ok(())
    }

    pub(crate) const fn terminal_mmio_report_pending(&self) -> bool {
        self.terminal_mmio_report.is_some()
    }

    pub(crate) fn take_terminal_mmio_report(&mut self) -> Option<crate::vm::UnhandledMmioReport> {
        self.terminal_mmio_report.take()
    }

    pub(super) fn arm_reap_publication(
        &mut self,
        thread: ThreadId,
        reason: crate::vm::endpoint_state::ClosureReason,
    ) -> Result<(), ()> {
        match &mut self.reap {
            ReapOwnership::Installed(publication) => publication.arm(thread, reason),
            ReapOwnership::None => Err(()),
        }
    }

    pub(crate) fn take_reap_publication(
        &mut self,
    ) -> Option<crate::task::thread::ThreadRetirement> {
        let ReapOwnership::Installed(publication) =
            core::mem::replace(&mut self.reap, ReapOwnership::None)
        else {
            return None;
        };
        if !publication.is_armed() {
            self.reap = ReapOwnership::Installed(publication);
            return None;
        }
        Some(crate::task::thread::ThreadRetirement::from_box(publication))
    }

    /// Owns the non-runnable validation execution and its interrupt model.
    /// Both allocations are complete before active-vCPU publication.
    pub(crate) fn try_timer_validation(
        hardware: crate::hal::vm::VcpuHardwareState,
        interrupts: crate::vm::VmInterruptController,
    ) -> Result<alloc::boxed::Box<Self>, hyper::mm::AllocationError> {
        let interrupts = hyper::mm::try_box(interrupts)?;
        hyper::mm::try_box(Self {
            vm: VcpuVm::TimerValidation { interrupts },
            instruction_context: hyper_vm_models::translation::GuestInstructionContext::new(),
            terminal_mmio_report: None,
            reap: ReapOwnership::None,
            vcpu_id: 0,
            hardware,
        })
    }
}

impl crate::task::thread::ExternalThreadExecutionLifecycle for VcpuExecution {
    fn take_thread_retirement(&mut self) -> Option<crate::task::thread::ThreadRetirement> {
        self.take_reap_publication()
    }
}

/// Exact terminal publication extracted only after scheduler detachment.
#[must_use = "the canonical vCPU terminal transition must be published"]
pub(crate) struct VcpuReapPublication {
    owner: hyper::mm::FallibleArc<crate::vm::installed::InstalledMachine>,
    endpoint: hyper::mm::FallibleArc<crate::vm::endpoint::VcpuEndpoint>,
    vcpu: u32,
    terminal: Option<(ThreadId, crate::vm::endpoint_state::ClosureReason)>,
}

impl VcpuReapPublication {
    const fn new(
        endpoint: hyper::mm::FallibleArc<crate::vm::endpoint::VcpuEndpoint>,
        vcpu: u32,
        owner: hyper::mm::FallibleArc<crate::vm::installed::InstalledMachine>,
    ) -> Self {
        Self {
            owner,
            endpoint,
            vcpu,
            terminal: None,
        }
    }

    fn arm(
        &mut self,
        thread: ThreadId,
        reason: crate::vm::endpoint_state::ClosureReason,
    ) -> Result<(), ()> {
        if self.terminal.replace((thread, reason)).is_some() {
            return Err(());
        }
        Ok(())
    }

    const fn is_armed(&self) -> bool {
        self.terminal.is_some()
    }

    /// Completes VM-owned terminal publication after the Thread is destroyed.
    fn publish(self) -> Result<(), crate::task::thread::ThreadRetirementError> {
        if !self.endpoint.is_valid_for(self.vcpu) {
            return Err(crate::task::thread::ThreadRetirementError::PublicationRejected);
        }
        let Some((thread, reason)) = self.terminal else {
            return Err(crate::task::thread::ThreadRetirementError::PublicationRejected);
        };
        self.endpoint
            .publish_reaped(thread, reason)
            .map_err(|_| crate::task::thread::ThreadRetirementError::PublicationRejected)?;
        if matches!(reason, crate::vm::endpoint_state::ClosureReason::Guest(_)) {
            self.owner.publish_vcpu_terminal();
        }
        Ok(())
    }
}

impl crate::task::thread::ThreadRetirementAction for VcpuReapPublication {
    fn complete(
        self: alloc::boxed::Box<Self>,
    ) -> Result<(), crate::task::thread::ThreadRetirementError> {
        self.publish()
    }
}
