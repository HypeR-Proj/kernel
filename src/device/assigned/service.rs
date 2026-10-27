// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability checks for physical assignment; no arbitrary host address input.

use super::{Assignment, DeviceAssignmentAuthority, PhysicalDevice};
use crate::mm::user_space::VmoObject;
use crate::process::Process;
use crate::vm::service::Error;
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
    crate::capability::Rights,
};
use {crate::object::KernelObject, crate::object::ObjectPublication};

#[derive(Debug)]
pub(crate) enum MatchError {
    Service(Error),
    Missing,
    Ambiguous,
}
impl From<Error> for MatchError {
    fn from(error: Error) -> Self {
        Self::Service(error)
    }
}
impl From<super::model::SelectionError> for MatchError {
    fn from(error: super::model::SelectionError) -> Self {
        match error {
            super::model::SelectionError::Missing => Self::Missing,
            super::model::SelectionError::Ambiguous => Self::Ambiguous,
        }
    }
}

pub(crate) fn claim_matching(
    process: &Process,
    authority: &InvocationReceiver,
    profile: u32,
    identity_kind: u32,
    identity: &str,
) -> Result<HandleValue, MatchError> {
    if identity.is_empty()
        || identity.len() > 512
        || identity.bytes().any(|byte| byte <= 32 || byte == 127)
        || !matches!(identity_kind, 1..=3)
        || (identity_kind == 3 && super::model::pci_identity(identity).is_none())
        || (identity_kind == 2
            && (!identity.starts_with('/')
                || identity.ends_with('/')
                || identity
                    .split('/')
                    .skip(1)
                    .any(|part| matches!(part, "" | "." | ".."))))
    {
        return Err(Error::InvalidArgument.into());
    }
    authority
        .resolve::<DeviceAssignmentAuthority>(Rights::ASSIGN_DEVICE)
        .map_err(crate::process::ProcessError::from)
        .map_err(Error::from)?;
    if !super::model::transport_profile(profile) {
        return Err(Error::NotSupported.into());
    }
    if !crate::hal::vm::supports_guest_device_assignment() {
        return Err(Error::NotSupported.into());
    }
    let reservation = process.reserve_handles::<1>().map_err(Error::from)?;
    let (reservation, prepared) = super::transaction::prepare(
        reservation,
        || -> Result<_, MatchError> {
            let object = PhysicalDevice::claim_matching(
                profile,
                identity_kind,
                identity,
                &process.resource_domain(),
            )?;
            let publication =
                ObjectPublication::try_new_boxed(object).map_err(|_| Error::NoMemory)?;
            let prepared = PreparedHandle::try_from_new_object(
                publication,
                Rights::TRANSFER
                    .union(Rights::DUPLICATE)
                    .union(Rights::INSPECT)
                    .union(Rights::ASSIGN_DEVICE)
                    .union(Rights::WAIT),
                HandleFlags::NONE,
            )
            .map_err(|_| Error::NoMemory)?;
            Ok(prepared)
        },
        |reservation| process.abort_handles(reservation),
    )?;
    process
        .publish_handles(reservation, [prepared])
        .map(|handles| handles[0])
        .map_err(|failure| MatchError::Service(failure.error.into()))
}

pub(crate) struct Info {
    pub(crate) device_id: u32,
    pub(crate) transport_version: u32,
    pub(crate) mmio_size: u64,
}
pub(crate) struct DmaExtent {
    pub(crate) physical_base: u64,
    pub(crate) length: u64,
}

pub(crate) fn claim(
    process: &Process,
    authority: &InvocationReceiver,
    index: u32,
) -> Result<HandleValue, Error> {
    let _authority = authority
        .resolve::<DeviceAssignmentAuthority>(Rights::ASSIGN_DEVICE)
        .map_err(crate::process::ProcessError::from)?;
    let reservation = process.reserve_handles::<1>()?;
    let (reservation, prepared) = super::transaction::prepare(
        reservation,
        || -> Result<_, Error> {
            let object = PhysicalDevice::claim(index as usize, &process.resource_domain())
                .map_err(classify)?;
            let publication =
                ObjectPublication::try_new_boxed(object).map_err(|_| Error::NoMemory)?;
            let prepared = PreparedHandle::try_from_new_object(
                publication,
                Rights::TRANSFER
                    .union(Rights::DUPLICATE)
                    .union(Rights::INSPECT)
                    .union(Rights::ASSIGN_DEVICE)
                    .union(Rights::WAIT),
                HandleFlags::NONE,
            )
            .map_err(|_| Error::NoMemory)?;
            Ok(prepared)
        },
        |reservation| process.abort_handles(reservation),
    )?;
    process
        .publish_handles(reservation, [prepared])
        .map(|handles| handles[0])
        .map_err(|failure| failure.error.into())
}
pub(crate) fn dma_fault_info(device: &InvocationReceiver) -> Result<[u8; 40], Error> {
    let device = device
        .resolve::<PhysicalDevice>(Rights::INSPECT.union(Rights::ASSIGN_DEVICE))
        .map_err(crate::process::ProcessError::from)?;
    Ok(device.object().events.snapshot())
}

pub(crate) fn profile_info(device: &InvocationReceiver) -> Result<[u8; 32], Error> {
    let device = device
        .resolve::<PhysicalDevice>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    Ok(device.object().profile_info())
}
pub(crate) fn resource_info(device: &InvocationReceiver, index: u32) -> Result<[u8; 32], Error> {
    let device = device
        .resolve::<PhysicalDevice>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?;
    device.object().resource_info(index).map_err(classify)
}

pub(crate) fn info(device: &InvocationReceiver) -> Result<Info, Error> {
    Ok(device
        .resolve::<PhysicalDevice>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)?
        .object()
        .info())
}
pub(crate) fn dma_extent(
    process: &Process,
    authority: &InvocationReceiver,
    vmo: HandleValue,
    offset: u64,
    length: u64,
) -> Result<DmaExtent, Error> {
    let _authority = authority
        .resolve::<DeviceAssignmentAuthority>(Rights::MAP_DMA)
        .map_err(crate::process::ProcessError::from)?;
    let vmo = process.resolve_handle::<VmoObject>(vmo, Rights::READ.union(Rights::MAP))?;
    let storage = vmo.object().writable().ok_or(Error::InvalidArgument)?;
    let first = super::model::contiguous_extent(
        offset,
        length,
        storage.size(),
        hyper::mm::PAGE_SIZE,
        |offset| {
            storage
                .resident_physical_page(offset)
                .ok()
                .map(|page| page.get())
        },
    )
    .map_err(|error| match error {
        super::model::ExtentError::Range => Error::InvalidArgument,
        super::model::ExtentError::Nonresident => Error::BadState,
        super::model::ExtentError::Noncontiguous => Error::NotSupported,
    })?;
    // This retained ordinary VMO never substitutes resident frames. This is
    // information for DT construction, not permission to start hardware DMA.
    Ok(DmaExtent {
        physical_base: first,
        length,
    })
}
pub(crate) fn assign(
    process: &Process,
    pending: &InvocationReceiver,
    device: HandleValue,
    base: u64,
    irq: u32,
) -> Result<(), Error> {
    let pending = pending
        .resolve::<crate::vm::objects::PendingVirtualMachine>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let device = process.resolve_handle::<PhysicalDevice>(device, Rights::ASSIGN_DEVICE)?;
    let assignment = Assignment::new(
        device.into_operation_pin().into_vm_device_binding(),
        base,
        irq,
    )
    .map_err(classify)?;
    pending
        .object()
        .assign_physical(assignment)
        .map_err(Into::into)
}
pub(crate) const fn classify(error: super::Error) -> Error {
    match error {
        super::Error::Unsupported => Error::NotSupported,
        super::Error::InvalidArgument => Error::InvalidArgument,
        super::Error::Resource => Error::NoMemory,
        super::Error::Busy => Error::Busy,
        super::Error::BadState | super::Error::Interrupt | super::Error::Quarantined => {
            Error::BadState
        }
    }
}

pub(crate) fn firmware_read(
    authority: &InvocationReceiver,
    node: u32,
    field: u32,
    name: &str,
) -> Result<alloc::vec::Vec<u8>, MatchError> {
    authority
        .resolve::<DeviceAssignmentAuthority>(Rights::INSPECT)
        .map_err(crate::process::ProcessError::from)
        .map_err(Error::from)?;
    super::super::platform_bus::catalogue()
        .map_err(classify)?
        .read(node, field, name)
}

pub(crate) fn claim_bundle(
    process: &Process,
    authority: &InvocationReceiver,
    entries: &[(u32, u32, u64)],
    irq_node: u32,
) -> Result<HandleValue, Error> {
    authority
        .resolve::<DeviceAssignmentAuthority>(Rights::ASSIGN_DEVICE)
        .map_err(crate::process::ProcessError::from)?;
    let reservation = process.reserve_handles::<1>()?;
    let (reservation, prepared) = super::transaction::prepare(
        reservation,
        || -> Result<_, Error> {
            let object =
                PhysicalDevice::claim_bundle(entries, irq_node, &process.resource_domain())
                    .map_err(classify)?;
            let publication =
                ObjectPublication::try_new_boxed(object).map_err(|_| Error::NoMemory)?;
            PreparedHandle::try_from_new_object(
                publication,
                PhysicalDevice::SUPPORTED_RIGHTS,
                HandleFlags::NONE,
            )
            .map_err(|_| Error::NoMemory)
        },
        |reservation| process.abort_handles(reservation),
    )?;
    process
        .publish_handles(reservation, [prepared])
        .map(|handles| handles[0])
        .map_err(|failure| failure.error.into())
}
pub(crate) fn mmio(
    device: &InvocationReceiver,
    offset: u64,
    width: u32,
    write: bool,
    value: u64,
) -> Result<u64, Error> {
    device
        .resolve::<PhysicalDevice>(if write { Rights::WRITE } else { Rights::READ })
        .map_err(crate::process::ProcessError::from)?
        .object()
        .mmio(offset, width, write, value)
        .map_err(classify)
}
pub(crate) fn irq_pending(device: &InvocationReceiver) -> Result<u64, Error> {
    device
        .resolve::<PhysicalDevice>(Rights::WAIT)
        .map_err(crate::process::ProcessError::from)?
        .object()
        .irq_pending()
        .map_err(classify)
}
pub(crate) fn irq_complete(
    device: &InvocationReceiver,
    sequence: u64,
    asserted: bool,
) -> Result<(), Error> {
    device
        .resolve::<PhysicalDevice>(Rights::ACK_INTERRUPT)
        .map_err(crate::process::ProcessError::from)?
        .object()
        .irq_complete(sequence, asserted)
        .map_err(classify)
}
