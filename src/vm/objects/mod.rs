// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Handle-visible VM objects and their shared construction errors.

mod authority;
mod guest_memory;
pub(crate) mod guest_memory_access;
pub(crate) use guest_memory::GuestMemoryObject;
mod installed;
mod pending;

pub(crate) use authority::{VirtualMachineCreationAuthority, VirtualMachineCreationLease};
pub(crate) use installed::{VirtualCpuObject, VirtualMachineObject};
pub(crate) use pending::PendingVirtualMachine;
pub(crate) use {
    crate::vm::installed::VirtualCpuSnapshot, crate::vm::installed::VirtualMachineConfiguration,
    crate::vm::installed::VirtualMachineSnapshot,
};

use crate::mm::user_space::MemoryObjectError;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectCreationError,
    crate::object::object_allocation_size,
};

/// Immutable initial machine state for the boot vCPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VirtualCpuBootstrap {
    pub(crate) entry: u64,
    pub(crate) stack: u64,
    pub(crate) arguments: [u64; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Allocation,
    BadState,
    InvalidConfiguration,
    Memory(MemoryObjectError),
    MemoryLayout(super::memory::Error),
    Object(ObjectCreationError),
    Registry(super::registry::Error),
    Resource(ResourceError),
    Scheduler(crate::task::scheduler::Error),
    UnsupportedArchitecture,
    VirtualDevice(super::device::Error),
    VirtualInterrupt(crate::hal::vm::InterruptError),
    VirtualSerial(crate::vm::serial::Error),
}

impl From<MemoryObjectError> for Error {
    fn from(error: MemoryObjectError) -> Self {
        Self::Memory(error)
    }
}

impl From<super::memory::Error> for Error {
    fn from(error: super::memory::Error) -> Self {
        Self::MemoryLayout(error)
    }
}

impl From<ObjectCreationError> for Error {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<super::registry::Error> for Error {
    fn from(error: super::registry::Error) -> Self {
        Self::Registry(error)
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<crate::task::scheduler::Error> for Error {
    fn from(error: crate::task::scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

impl From<super::registry::VcpuPreparationError> for Error {
    fn from(error: super::registry::VcpuPreparationError) -> Self {
        match error {
            super::registry::VcpuPreparationError::Registry(error) => Self::Registry(error),
            super::registry::VcpuPreparationError::Scheduler(error) => Self::Scheduler(error),
        }
    }
}

impl From<super::device::Error> for Error {
    fn from(error: super::device::Error) -> Self {
        Self::VirtualDevice(error)
    }
}

impl From<crate::hal::vm::InterruptError> for Error {
    fn from(error: crate::hal::vm::InterruptError) -> Self {
        Self::VirtualInterrupt(error)
    }
}

impl From<crate::vm::serial::Error> for Error {
    fn from(error: crate::vm::serial::Error) -> Self {
        Self::VirtualSerial(error)
    }
}

impl From<super::installed::Error> for Error {
    fn from(error: super::installed::Error) -> Self {
        match error {
            super::installed::Error::Allocation => Self::Allocation,
            super::installed::Error::BadState => Self::BadState,
            super::installed::Error::Scheduler(error) => Self::Scheduler(error),
        }
    }
}

pub(super) fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, Error> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(Error::Allocation)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}
