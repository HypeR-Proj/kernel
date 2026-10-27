// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared lifecycle model for one installed virtual machine.
//!
//! Registry hardware ownership and userspace handle ownership meet here but
//! never own each other. The registry and every handle retain this aggregate;
//! the aggregate owns no registry machine lease, so hardware retirement can
//! still acquire unique registry ownership while diagnostic handles survive.

use alloc::vec::Vec;

use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;

use crate::task::thread::ThreadId;
use {crate::object::SignalMask, crate::object::SignalState};

use super::endpoint::VcpuEndpoint;
use super::registry::{VmControl, VmId, VmLifecycleResources};

type RuntimeLock = InterruptSpinLock<RuntimeState, crate::hal::irq::LocalMask>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualMachineConfiguration {
    pub(crate) guest_physical_base: u64,
    pub(crate) memory_size: u64,
    pub(crate) vcpu_count: u32,
    pub(crate) architecture: u32,
    pub(crate) platform_profile: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualMachineSnapshot {
    pub(crate) phase: u32,
    pub(crate) boot_vcpu: u64,
    pub(crate) resident_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualCpuSnapshot {
    pub(crate) id: u32,
    pub(crate) phase: u32,
    pub(crate) thread: u64,
    pub(crate) terminal_reason: u32,
    pub(crate) host_cpu: Option<u32>,
    pub(crate) migration_target: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Allocation,
    BadState,
    Scheduler(crate::task::scheduler::Error),
}

enum RuntimeState {
    Uninstalled,
    Installed {
        id: VmId,
        control: Option<VmControl>,
    },
    Running {
        id: VmId,
        control: Option<VmControl>,
    },
    Stopping {
        id: VmId,
    },
    Stopped,
}

/// Handle-visible state and VM-owned endpoints for one installed incarnation.
pub(crate) struct InstalledMachine {
    configuration: VirtualMachineConfiguration,
    state: RuntimeLock,
    vm_signals: SignalState,
    power: InterruptSpinLock<hyper_vm_models::arm::psci::PowerState, crate::hal::irq::LocalMask>,
    mmio_regions: InterruptSpinLock<[Option<user_mmio::Region>; 8], crate::hal::irq::LocalMask>,
    endpoints: Vec<FallibleArc<VcpuEndpoint>>,
    resources: VmLifecycleResources,
}

impl InstalledMachine {
    pub(super) fn try_new(
        configuration: VirtualMachineConfiguration,
        resources: VmLifecycleResources,
    ) -> Result<FallibleArc<Self>, Error> {
        let count = usize::try_from(configuration.vcpu_count).map_err(|_| Error::Allocation)?;
        if count == 0 {
            return Err(Error::BadState);
        }
        let mut endpoints = Vec::new();
        endpoints
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        // VmLifecycleResources admitted exactly `count` retained endpoint-owner
        // slots. Reject a future Vec growth-policy change before any endpoint
        // or aggregate is published under a smaller metadata charge.
        if endpoints.capacity() != count {
            return Err(Error::Allocation);
        }
        for id in 0..configuration.vcpu_count {
            let endpoint = VcpuEndpoint::try_new(id).map_err(|_| Error::Allocation)?;
            endpoints.push(FallibleArc::try_new(endpoint).map_err(|_| Error::Allocation)?);
        }
        FallibleArc::try_new(Self {
            configuration,
            state: RuntimeLock::new(RuntimeState::Uninstalled),
            vm_signals: SignalState::new(),
            power: InterruptSpinLock::new(
                hyper_vm_models::arm::psci::PowerState::new(count).ok_or(Error::BadState)?,
            ),
            mmio_regions: InterruptSpinLock::new([None; 8]),
            endpoints,
            resources,
        })
        .map_err(|_| Error::Allocation)
    }

    pub(super) fn endpoint(&self, id: u32) -> Result<&FallibleArc<VcpuEndpoint>, Error> {
        let index = usize::try_from(id).map_err(|_| Error::BadState)?;
        let endpoint = self.endpoints.get(index).ok_or(Error::BadState)?;
        endpoint
            .is_valid_for(id)
            .then_some(endpoint)
            .ok_or(Error::BadState)
    }

    pub(super) fn endpoints(&self) -> &[FallibleArc<VcpuEndpoint>] {
        &self.endpoints
    }

    pub(super) fn reserve_vcpu_runtime(
        &self,
    ) -> Result<crate::task::thread::ThreadResourceOwnership, super::registry::Error> {
        self.resources.reserve_vcpu_runtime()
    }

    /// Installs the sole lifecycle authority after registry publication.
    pub(super) fn publish_installed(&self, id: VmId, control: VmControl) {
        self.state.with(|state| {
            if !matches!(state, RuntimeState::Uninstalled) {
                hyper::debug::invariant_failure("vm::installed::publish_installed invariant");
            }
            *state = RuntimeState::Installed {
                id,
                control: Some(control),
            };
        });
    }

    /// Makes one dormant endpoint runnable exactly once.
    pub(super) fn start_vcpu(&self, id: u32) -> Result<(), Error> {
        self.state.with(|state| match state {
            RuntimeState::Installed { id: vm_id, control } => {
                if id != 0 {
                    return Err(Error::BadState);
                }
                let endpoint = self.endpoint(id)?;
                if endpoint.lifecycle().map_err(|_| Error::BadState)?
                    != super::endpoint_state::Lifecycle::Dormant
                {
                    return Err(Error::BadState);
                }
                self.power
                    .with(|power| power.boot())
                    .map_err(|_| Error::BadState)?;
                // Power-on is committed. The lifecycle lock excludes stop,
                // and no vCPU can run before this first start. Failure now
                // proves internal corruption, not a reversible start error.
                if endpoint.start().is_err() {
                    crate::diagnostics::crash::fatal(format_args!(
                        "HypeR: boot vCPU start failed after power-on commit"
                    ));
                }
                let vm_id = *vm_id;
                let control = match control.take() {
                    Some(control) => control,
                    None => hyper::debug::invariant_failure("vm::installed::start_vcpu invariant"),
                };
                *state = RuntimeState::Running {
                    id: vm_id,
                    control: Some(control),
                };
                Ok(())
            }
            RuntimeState::Running { .. } => Err(Error::BadState),
            RuntimeState::Uninstalled | RuntimeState::Stopping { .. } | RuntimeState::Stopped => {
                Err(Error::BadState)
            }
        })
    }

    /// Serializes placement admission against whole-VM stop. The scheduler
    /// owns source save/target publication and arbitrates concurrent thread exit.
    pub(super) fn set_vcpu_affinity(
        &self,
        id: u32,
        affinity: crate::task::policy::CpuMask,
    ) -> Result<(), Error> {
        self.state.with(|state| {
            if !matches!(
                state,
                RuntimeState::Installed { .. } | RuntimeState::Running { .. }
            ) {
                return Err(Error::BadState);
            }
            let endpoint = self.endpoint(id)?;
            if !matches!(
                endpoint.lifecycle().map_err(|_| Error::BadState)?,
                super::endpoint_state::Lifecycle::Dormant
                    | super::endpoint_state::Lifecycle::Started
            ) {
                return Err(Error::BadState);
            }
            let thread = endpoint.thread().ok_or(Error::BadState)?;
            crate::task::scheduler::set_thread_affinity(thread, affinity)
                .map(|_| ())
                .map_err(Error::Scheduler)
        })
    }

    pub(super) fn vcpu_scheduling(
        &self,
        id: u32,
    ) -> Result<crate::task::policy::SchedulingParameters, Error> {
        self.state.with(|state| {
            if !matches!(
                state,
                RuntimeState::Installed { .. } | RuntimeState::Running { .. }
            ) {
                return Err(Error::BadState);
            }
            let thread = self.endpoint(id)?.thread().ok_or(Error::BadState)?;
            crate::task::scheduler::scheduling_snapshot(thread).map_err(Error::Scheduler)
        })
    }

    pub(super) fn set_vcpu_scheduling(
        &self,
        id: u32,
        parameters: crate::task::policy::SchedulingParameters,
        authority: crate::task::scheduling_authority::SchedulingAuthority,
    ) -> Result<(), Error> {
        self.state.with(|state| {
            if !matches!(
                state,
                RuntimeState::Installed { .. } | RuntimeState::Running { .. }
            ) {
                return Err(Error::BadState);
            }
            let thread = self.endpoint(id)?.thread().ok_or(Error::BadState)?;
            crate::task::scheduler::configure_scheduling(
                thread,
                parameters,
                authority,
                crate::task::scheduling_authority::SchedulingLimits::SYSTEM,
            )
            .map_err(Error::Scheduler)
        })
    }

    fn take_stop_control(&self) -> Option<VmControl> {
        self.state.with(|state| match state {
            RuntimeState::Installed { id, control } | RuntimeState::Running { id, control } => {
                let control = match control.take() {
                    Some(control) => control,
                    None => hyper::debug::invariant_failure(
                        "vm::installed::take_stop_control invariant",
                    ),
                };
                *state = RuntimeState::Stopping { id: *id };
                Some(control)
            }
            RuntimeState::Uninstalled | RuntimeState::Stopping { .. } | RuntimeState::Stopped => {
                None
            }
        })
    }

    pub(crate) fn request_stop(owner: &FallibleArc<Self>) {
        if let Some(control) = owner.take_stop_control() {
            super::lifecycle::enqueue(owner.clone(), control);
        }
    }

    pub(super) fn publish_stopped(&self) {
        self.state.with(|state| {
            let old = core::mem::replace(state, RuntimeState::Uninstalled);
            let RuntimeState::Stopping { id } = old else {
                hyper::debug::invariant_failure("vm::installed::publish_stopped invariant")
            };
            let _ = id;
            *state = RuntimeState::Stopped;
        });
        let terminated = SignalMask::from_trusted_bits(
            hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_MACHINE_TERMINATED,
        );
        if self
            .vm_signals
            .update(SignalMask::EMPTY, terminated)
            .is_err()
        {
            hyper::debug::invariant_failure("vm::installed::publish_stopped invariant");
        }
    }

    pub(crate) const fn configuration(&self) -> VirtualMachineConfiguration {
        self.configuration
    }

    pub(crate) const fn signal_state(&self) -> &SignalState {
        &self.vm_signals
    }

    pub(super) fn vcpu_signal_state(&self, id: u32) -> Result<&SignalState, Error> {
        self.endpoint(id).map(|endpoint| endpoint.signals())
    }

    fn vcpu_snapshot(&self, id: u32) -> Result<VirtualCpuSnapshot, Error> {
        let endpoint = self.endpoint(id)?;
        let thread = endpoint.thread().map_or(0, ThreadId::get);
        let lifecycle = endpoint.lifecycle().map_err(|_| Error::BadState)?;
        let (phase, terminal_reason) = match lifecycle {
            super::endpoint_state::Lifecycle::Unbound => (
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_PHASE_STOPPED as u32,
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_NONE as u32,
            ),
            super::endpoint_state::Lifecycle::Dormant => (
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_PHASE_DORMANT as u32,
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_NONE as u32,
            ),
            super::endpoint_state::Lifecycle::Started
            | super::endpoint_state::Lifecycle::GuestTerminal(_)
            | super::endpoint_state::Lifecycle::StopRequested(_)
            | super::endpoint_state::Lifecycle::HardwareDetached(_) => (
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_PHASE_STARTED as u32,
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_NONE as u32,
            ),
            super::endpoint_state::Lifecycle::Reaped(reason) => (
                hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_PHASE_STOPPED as u32,
                encode_terminal_reason(reason),
            ),
        };
        // This is an observation, not a lifetime claim: concurrent retirement
        // may remove the scheduler thread after endpoint inspection.
        let placement = endpoint
            .thread()
            .and_then(|thread| crate::task::scheduler::thread_migration_state(thread).ok());
        Ok(VirtualCpuSnapshot {
            host_cpu: placement.map(|(cpu, _)| cpu.get() as u32),
            migration_target: placement.and_then(|(_, target)| target.map(|cpu| cpu.get() as u32)),
            id,
            phase,
            thread,
            terminal_reason,
        })
    }

    pub(crate) fn snapshot_vcpu(&self, id: u32) -> VirtualCpuSnapshot {
        match self.vcpu_snapshot(id) {
            Ok(snapshot) => snapshot,
            Err(_) => hyper::debug::invariant_failure("vm::installed::snapshot_vcpu invariant"),
        }
    }

    pub(crate) fn snapshot(&self) -> VirtualMachineSnapshot {
        let boot_vcpu = self.snapshot_vcpu(0).thread;
        self.state.with(|state| match state {
            RuntimeState::Uninstalled => VirtualMachineSnapshot {
                phase: hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_STOPPED as u32,
                boot_vcpu: 0,
                resident_memory_bytes: None,
            },
            RuntimeState::Installed { .. } => VirtualMachineSnapshot {
                phase: hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_INSTALLED as u32,
                boot_vcpu,
                resident_memory_bytes: None,
            },
            RuntimeState::Running { .. } => VirtualMachineSnapshot {
                phase: hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_RUNNING as u32,
                boot_vcpu,
                resident_memory_bytes: None,
            },
            RuntimeState::Stopping { .. } => VirtualMachineSnapshot {
                phase: hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_STOPPING as u32,
                boot_vcpu,
                resident_memory_bytes: None,
            },
            RuntimeState::Stopped => VirtualMachineSnapshot {
                phase: hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_STOPPED as u32,
                boot_vcpu,
                resident_memory_bytes: None,
            },
        })
    }

    pub(crate) fn information_snapshot(&self) -> VirtualMachineSnapshot {
        let mut snapshot = self.snapshot();
        let id = self.state.with(|state| match state {
            RuntimeState::Installed { id, .. }
            | RuntimeState::Running { id, .. }
            | RuntimeState::Stopping { id } => Some(*id),
            RuntimeState::Uninstalled | RuntimeState::Stopped => None,
        });
        // Retain RAM leases under the address-space lock, then release the
        // registry binding and all outer locks before walking resident pages.
        snapshot.resident_memory_bytes = id.and_then(|id| {
            super::registry::with_binding(id, |binding| {
                binding.with_address_space(|space| space.resident_memory())
            })
            .ok()?
            .bytes()
            .ok()
        });
        snapshot
    }
}

const fn encode_terminal_reason(reason: super::endpoint_state::ClosureReason) -> u32 {
    match reason {
        super::endpoint_state::ClosureReason::Guest(
            super::endpoint_state::TerminalReason::MemoryFault,
        ) => hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_MEMORY_FAULT as u32,
        super::endpoint_state::ClosureReason::Guest(
            super::endpoint_state::TerminalReason::Mmio,
        ) => hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_MMIO as u32,
        super::endpoint_state::ClosureReason::Guest(
            super::endpoint_state::TerminalReason::Synchronous,
        ) => hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_SYNCHRONOUS as u32,
        super::endpoint_state::ClosureReason::Administrative(_) => {
            hyper::abi::native::HYPER_NATIVE_VIRTUAL_CPU_TERMINAL_ADMINISTRATIVE as u32
        }
    }
}

#[path = "power.rs"]
mod power;
#[path = "user_mmio.rs"]
mod user_mmio;

impl InstalledMachine {
    /// Serializes immutable device-topology publication with first vCPU start.
    pub(in crate::vm) fn with_io_install<R>(
        &self,
        operation: impl FnOnce(VmId) -> Result<R, super::io::Error>,
    ) -> Result<R, super::io::Error> {
        self.state.with(|state| match state {
            RuntimeState::Installed { id, .. } => operation(*id),
            _ => Err(super::io::Error::BadState),
        })
    }
    pub(in crate::vm) fn io_install_id(&self) -> Result<VmId, super::io::Error> {
        self.with_io_install(Ok)
    }

    /// Publish only dynamic I/O routes or admitted memory into the platform's
    /// reserved apertures. Unlike immutable topology installation this permits
    /// a running backend, while excluding stop/retirement across the commit.
    /// The callback must be nonblocking and must prepare allocations beforehand.
    pub(in crate::vm) fn with_io_update<R>(
        &self,
        operation: impl FnOnce(VmId) -> Result<R, super::io::Error>,
    ) -> Result<R, super::io::Error> {
        self.state.with(|state| match state {
            RuntimeState::Installed { id, .. } | RuntimeState::Running { id, .. } => operation(*id),
            _ => Err(super::io::Error::BadState),
        })
    }
    pub(in crate::vm) fn io_update_id(&self) -> Result<VmId, super::io::Error> {
        self.with_io_update(Ok)
    }
}
