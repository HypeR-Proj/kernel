// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise retained mapping permissions through the real Native service path.

use hyper::mm::PAGE_SIZE;

use super::{Error, IMAGE_BASE};
use crate::accounting::ResourceDomain;
use crate::object::KernelObject;
use crate::process::Process;
use {crate::capability::InvocationReceiver, crate::capability::Rights};
use {
    crate::mm::user_space::AddressSpaceError, crate::mm::user_space::GuestMemoryBacking,
    crate::mm::user_space::MachineError, crate::mm::user_space::MemoryServiceError,
    crate::mm::user_space::Permissions, crate::mm::user_space::UserAddress,
    crate::mm::user_space::UserSlice, crate::mm::user_space::VmoObject,
    crate::mm::user_space::create_vmo, crate::mm::user_space::map_vmo,
    crate::mm::user_space::protect, crate::mm::user_space::unmap,
};

const BASE: u64 = IMAGE_BASE + PAGE_SIZE * 5;

pub(super) fn verify(
    process: &Process,
    domain: &ResourceDomain,
    vmar: &InvocationReceiver,
) -> Result<(), Error> {
    let full = create_vmo(process, PAGE_SIZE * 3).map_err(|_| Error::State(90))?;
    let source = process.resolve_handle::<VmoObject>(full, Rights::READ)?;
    let observed = VmoObject::from_read_only_native(
        source.object().writable_clone().ok_or(Error::State(102))?,
        domain,
    )
    .map_err(|_| Error::State(102))?;
    if observed.supported_rights().contains(Rights::WRITE)
        || observed.writable().is_some()
        || GuestMemoryBacking::try_from_vmo(&observed).is_ok()
    {
        return Err(Error::State(103));
    }
    drop(source);
    let observed = process.create_object(observed, Rights::READ.union(Rights::MAP))?;
    let read_only = process.duplicate_handle(full, Rights::READ.union(Rights::MAP))?;
    let map_only = process.duplicate_handle(full, Rights::MAP)?;

    // A no-access mapping must not recover READ from the backing object's type.
    if map_vmo(
        process,
        vmar,
        map_only,
        0,
        BASE,
        PAGE_SIZE,
        Permissions::NONE,
    )
    .is_ok()
    {
        return Err(Error::State(91));
    }
    process.close_handle(map_only)?;
    map_vmo(
        process,
        vmar,
        read_only,
        0,
        BASE,
        PAGE_SIZE * 3,
        Permissions::read_only(),
    )
    .map_err(|_| Error::State(92))?;
    process.close_handle(read_only)?;

    // Splitting a mapping must preserve the original handle's ceiling for all
    // fragments. Closing that handle must not erase the ceiling either.
    protect(
        process,
        vmar,
        BASE + PAGE_SIZE,
        PAGE_SIZE,
        Permissions::NONE,
    )
    .map_err(|_| Error::State(93))?;
    for index in 0..3 {
        if !matches!(
            protect(
                process,
                vmar,
                BASE + index * PAGE_SIZE,
                PAGE_SIZE,
                Permissions::read_write(),
            ),
            Err(MemoryServiceError::Machine(MachineError::Logical(
                AddressSpaceError::InvalidPermissions
            )))
        ) {
            return Err(Error::State(94));
        }
    }
    protect(
        process,
        vmar,
        BASE + PAGE_SIZE,
        PAGE_SIZE,
        Permissions::read_only(),
    )
    .map_err(|_| Error::State(95))?;
    let byte =
        UserSlice::new(UserAddress::new(BASE + PAGE_SIZE), 1).map_err(|_| Error::Construction)?;
    let mut contents = [0xff];
    process.copy_from_user(byte, &mut contents)?;
    if contents != [0] || process.copy_to_user(byte, &[0x5a]).is_ok() {
        return Err(Error::State(96));
    }

    // An independently authorized mapping can enable WRITE later; restricting
    // every mapping to its initial permissions would break the loader contract.
    unmap(process, vmar, BASE + PAGE_SIZE, PAGE_SIZE).map_err(|_| Error::State(97))?;
    map_vmo(
        process,
        vmar,
        full,
        PAGE_SIZE,
        BASE + PAGE_SIZE,
        PAGE_SIZE,
        Permissions::NONE,
    )
    .map_err(|_| Error::State(98))?;
    process.close_handle(full)?;
    protect(
        process,
        vmar,
        BASE + PAGE_SIZE,
        PAGE_SIZE,
        Permissions::read_write(),
    )
    .map_err(|_| Error::State(99))?;
    process.copy_to_user(byte, &[0x5a])?;
    process.copy_from_user(byte, &mut contents)?;
    if contents != [0x5a] {
        return Err(Error::State(100));
    }
    unmap(process, vmar, BASE, PAGE_SIZE * 3).map_err(|_| Error::State(101))?;
    map_vmo(
        process,
        vmar,
        observed,
        0,
        BASE,
        PAGE_SIZE * 3,
        Permissions::read_only(),
    )
    .map_err(|_| Error::State(104))?;
    process.close_handle(observed)?;
    if !matches!(
        protect(
            process,
            vmar,
            BASE,
            PAGE_SIZE * 3,
            Permissions::read_write()
        ),
        Err(MemoryServiceError::Machine(MachineError::Logical(
            AddressSpaceError::InvalidPermissions
        )))
    ) {
        return Err(Error::State(105));
    }
    unmap(process, vmar, BASE, PAGE_SIZE * 3).map_err(|_| Error::State(106))?;
    Ok(())
}
