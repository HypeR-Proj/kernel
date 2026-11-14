// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real contiguous allocation rejects invalid input without spending quota.

use crate::abi::native::status::status_from_memory_object_error;
use crate::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::mm::user_space::{MemoryObjectError, VmoError, VmoObject};
use hyper::abi::native as abi;

pub(super) fn run() -> Result<(), ()> {
    // Logical permission ceilings are authority failures; unsupported shared
    // backing operations are capability gaps, while invalid copy access still
    // reports FAULT. Do not conflate the three at the syscall boundary.
    for (error, expected) in [
        (
            crate::mm::user_space::AddressSpaceError::InvalidPermissions,
            abi::HYPER_NATIVE_STATUS_ACCESS_DENIED,
        ),
        (
            crate::mm::user_space::AddressSpaceError::UnsupportedBacking,
            abi::HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        ),
        (
            crate::mm::user_space::AddressSpaceError::WriteDenied,
            abi::HYPER_NATIVE_STATUS_FAULT,
        ),
    ] {
        if crate::abi::native::status::status_from_logical_error(error) != expected {
            return Err(());
        }
    }
    let domain = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| ())?;
    let before = domain.usage();
    for size in [
        0,
        1,
        abi::HYPER_NATIVE_PAGE_SIZE - 1,
        abi::HYPER_NATIVE_PAGE_SIZE * 3,
        abi::HYPER_NATIVE_VMO_MAX_CONTIGUOUS_SIZE_BYTES * 2,
    ] {
        let error = match VmoObject::try_new_contiguous(size, &domain) {
            Ok(_) => return Err(()),
            Err(error) => error,
        };
        if status_from_memory_object_error(error) != abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT
            || domain.usage() != before
        {
            return Err(());
        }
    }
    let limited = ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 0),
    )
    .map_err(|_| ())?;
    let before_limited = limited.usage();
    let error = match VmoObject::try_new_contiguous(abi::HYPER_NATIVE_PAGE_SIZE, &limited) {
        Ok(_) => return Err(()),
        Err(error) => error,
    };
    if status_from_memory_object_error(error) != abi::HYPER_NATIVE_STATUS_RESOURCE_LIMIT
        || limited.usage() != before_limited
    {
        return Err(());
    }
    let allocated =
        VmoObject::try_new_contiguous(abi::HYPER_NATIVE_PAGE_SIZE, &domain).map_err(|_| ())?;
    if domain.usage().total(ResourceKind::CommittedPages) != 1 {
        return Err(());
    }
    drop(allocated);
    if domain.usage() != before
        || status_from_memory_object_error(MemoryObjectError::Vmo(VmoError::Allocation))
            != abi::HYPER_NATIVE_STATUS_NO_MEMORY
    {
        return Err(());
    }
    Ok(())
}
