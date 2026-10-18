// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise permission checks through the real process handle table.

use crate::kernel::{
    authority::Rights,
    device::assigned::{DeviceAssignmentAuthority, service},
    object::KernelObject,
    process::Process,
    vm::service::Error,
};

pub(super) fn verify(process: &Process) -> Result<(), &'static str> {
    let authority = DeviceAssignmentAuthority::try_new(&process.resource_domain())
        .map_err(|_| "device authority")?;
    let root = process
        .create_object(authority, DeviceAssignmentAuthority::SUPPORTED_RIGHTS)
        .map_err(|_| "authority publication")?;
    let inspect = process
        .duplicate_handle(root, Rights::INSPECT.union(Rights::DUPLICATE))
        .map_err(|_| "inspection delegation")?;
    let assign = process
        .duplicate_handle(root, Rights::ASSIGN_DEVICE)
        .map_err(|_| "assignment delegation")?;
    let map = process
        .duplicate_handle(root, Rights::MAP_DMA)
        .map_err(|_| "DMA delegation")?;
    for handle in [inspect, map] {
        if !matches!(
            service::claim(process, handle, u32::MAX),
            Err(Error::AccessDenied)
        ) || !matches!(
            service::claim_matching(process, handle, 1, 1, "virtio,mmio"),
            Err(service::MatchError::Service(Error::AccessDenied))
        ) || !matches!(
            service::claim_bundle(process, handle, &[], 0),
            Err(Error::AccessDenied)
        ) {
            return Err("discovery or address permission allowed claim");
        }
    }
    for handle in [inspect, assign] {
        // Use the authority itself in the VMO slot: access must fail before a
        // VMO lookup, independently of residency or available hardware.
        if !matches!(
            service::dma_extent(process, handle, root, 0, 4096),
            Err(Error::AccessDenied)
        ) {
            return Err("non-MAP_DMA authority disclosed DMA extent");
        }
    }
    for handle in [assign, map] {
        if !matches!(
            service::firmware_read(process, handle, 0, 0, ""),
            Err(service::MatchError::Service(Error::AccessDenied))
        ) {
            return Err("non-INSPECT authority allowed discovery");
        }
    }
    if process
        .duplicate_handle(inspect, Rights::ASSIGN_DEVICE)
        .is_ok()
        || process.duplicate_handle(inspect, Rights::MAP_DMA).is_ok()
    {
        return Err("attenuated authority regained rights");
    }
    for handle in [inspect, assign, map, root] {
        process
            .close_handle(handle)
            .map_err(|_| "close device authority")?;
    }
    Ok(())
}
