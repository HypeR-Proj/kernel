// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VM-lifetime and per-vCPU resource accounting ownership.

use hyper::mm::FallibleArc;

use super::Error;
use super::execution::VirtualMachine;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};

/// Linear accounting ownership for one reserved VM incarnation.
///
/// Reservation happens alongside logical VM identity reservation, before any
/// guest memory or scheduler object can be constructed. Ownership moves into
/// the installed registry aggregate and is released only after vCPU reaping,
/// stage-2 retirement, and final registry destruction have all completed.
#[must_use = "VM lifecycle resources must remain owned through retirement"]
pub(crate) struct VmLifecycleResources {
    _charge: CommittedCharge,
    domain: ResourceDomain,
}

impl VmLifecycleResources {
    pub(crate) fn try_reserve(domain: &ResourceDomain, vcpu_count: u32) -> Result<Self, Error> {
        let count = usize::try_from(vcpu_count).map_err(|_| Error::Allocation)?;
        if count == 0 {
            return Err(Error::UnknownVcpu);
        }
        let endpoint_slots = core::mem::size_of::<FallibleArc<crate::vm::endpoint::VcpuEndpoint>>()
            .checked_mul(count)
            .ok_or(Error::Allocation)?;
        let endpoint_allocations =
            FallibleArc::<crate::vm::endpoint::VcpuEndpoint>::allocation_size()
                .checked_mul(count)
                .ok_or(Error::Allocation)?;
        let timer_bytes = crate::time::IrqCallout::allocation_size()
            .checked_mul(count)
            .ok_or(Error::Allocation)?;
        let allocation_bytes = VirtualMachine::allocation_size()
            .checked_add(FallibleArc::<crate::vm::installed::InstalledMachine>::allocation_size())
            .and_then(|bytes| bytes.checked_add(endpoint_slots))
            .and_then(|bytes| bytes.checked_add(endpoint_allocations))
            .and_then(|bytes| bytes.checked_add(timer_bytes))
            .and_then(|bytes| bytes.checked_add(crate::vm::device::dynamic_allocation_bytes()))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(Error::Allocation)?;
        let count = u64::from(vcpu_count);
        let timers = count
            .checked_add(crate::vm::device::timer_count())
            .ok_or(Error::Allocation)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, allocation_bytes)
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::Timers, timers)
                    .with(ResourceKind::VirtualMachines, 1)
                    .with(ResourceKind::VirtualCpus, count),
            )?
            .commit();
        Ok(Self {
            _charge: charge,
            domain: domain.clone(),
        })
    }

    pub(in crate::vm) fn reserve_vcpu_runtime(
        &self,
    ) -> Result<crate::task::thread::ThreadResourceOwnership, Error> {
        let payload_bytes =
            crate::vm::vcpu::VcpuExecution::allocation_size().ok_or(Error::Allocation)?;
        let bytes = crate::task::thread::Thread::external_execution_allocation_size(payload_bytes)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(Error::Allocation)?;
        let stack_pages = u64::try_from(crate::mm::stack::thread_stack_bytes())
            .ok()
            .and_then(|bytes| bytes.checked_div(hyper::mm::PAGE_SIZE))
            .ok_or(Error::Allocation)?;
        let execution_charge = self
            .domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, bytes)
                    .with(ResourceKind::CommittedPages, stack_pages)
                    .with(ResourceKind::Threads, 1),
            )?
            .commit();
        let retirement_bytes =
            u64::try_from(crate::vm::vcpu::VcpuExecution::retirement_allocation_size())
                .map_err(|_| Error::Allocation)?;
        let retirement_charge = self
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, retirement_bytes))?
            .commit();
        let object_bytes = crate::task::system_thread_object_allocation_size()
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(Error::Allocation)?;
        let object_charge = self
            .domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, object_bytes)
                    .with(ResourceKind::KernelObjects, 1),
            )?
            .commit();
        Ok(crate::task::thread::ThreadResourceOwnership::new(
            execution_charge,
            retirement_charge,
            object_charge,
        ))
    }

    pub(crate) fn reserve_interrupt_controller(
        &self,
        allocation_bytes: usize,
    ) -> Result<Option<CommittedCharge>, Error> {
        if allocation_bytes == 0 {
            return Ok(None);
        }
        let allocation_bytes = u64::try_from(allocation_bytes).map_err(|_| Error::Allocation)?;
        Ok(Some(
            self.domain
                .reserve(
                    ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, allocation_bytes),
                )?
                .commit(),
        ))
    }
}
