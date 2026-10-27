// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercises production native-page allocation and typed user-copy plumbing.

use hyper::mm::PAGE_SIZE;

use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {
    crate::mm::user_space::DomainAccount, crate::mm::user_space::KernelPageBackend,
    crate::mm::user_space::Permissions, crate::mm::user_space::UserAddress,
    crate::mm::user_space::UserAddressSpace, crate::mm::user_space::UserSlice,
    crate::mm::user_space::WritableVmo, crate::mm::user_space::address_window,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Contract,
    Copy,
    Payload,
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Contract)?;
    let account = DomainAccount::new(domain);
    let backend = KernelPageBackend;
    let plan = crate::hal::user::address_space_plan().map_err(|_| Error::Contract)?;
    let window = address_window(&plan).map_err(|_| Error::Contract)?;
    let range =
        UserSlice::new(UserAddress::new(0x20_0000), PAGE_SIZE).map_err(|_| Error::Contract)?;
    let address_space = UserAddressSpace::try_new(window, range, backend, account.clone())
        .map_err(|_| Error::Contract)?;
    let vmo = WritableVmo::try_new(PAGE_SIZE, backend, account).map_err(|_| Error::Contract)?;
    vmo.populate(0, PAGE_SIZE).map_err(|_| Error::Contract)?;
    let prepared = address_space
        .prepare_map_writable(
            address_space.root_vmar(),
            range,
            vmo,
            0,
            Permissions::read_write(),
            Permissions::read_write(),
        )
        .map_err(|_| Error::Contract)?;
    let committed = prepared.commit_for_test().map_err(|_| Error::Contract)?;
    // SAFETY: This portable self-test never installs the logical mapping into
    // a machine translation root, so there is no TLB-visible retired epoch.
    unsafe { committed.complete_retirement_for_test() };

    let payload = *b"checked app-memory boundary";
    let copy_range = UserSlice::new(UserAddress::new(0x20_000d), payload.len() as u64)
        .map_err(|_| Error::Contract)?;
    address_space
        .copy_to_user(copy_range, &payload)
        .map_err(|_| Error::Copy)?;
    let mut copied = [0; 27];
    address_space
        .copy_from_user(copy_range, &mut copied)
        .map_err(|_| Error::Copy)?;
    if copied != payload {
        return Err(Error::Payload);
    }
    crate::mm::user_space::run_dormant_self_test().map_err(|_| Error::Contract)?;
    Ok(())
}
