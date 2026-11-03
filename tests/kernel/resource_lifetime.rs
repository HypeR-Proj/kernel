// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Quota follows retained DMA owners and failed physical-device preparation.

use crate::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::device::dma::{BackendMemoryLease, Buffer};
use crate::mm::user_space::{GuestMemoryBacking, MemoryObjectError, VmoObject};

pub(super) enum Error {
    Setup,
    Quota,
    Lifetime,
    Quiescence(super::support::QuiescenceError),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Setup => formatter.write_str("Setup"),
            Self::Quota => formatter.write_str("Quota"),
            Self::Lifetime => formatter.write_str("Lifetime"),
            Self::Quiescence(error) => formatter.debug_tuple("Quiescence").field(error).finish(),
        }
    }
}

pub(super) fn run() -> Result<(), Error> {
    let root = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Setup)?;
    let first = root
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::DmaMappings, 1))
        .map_err(|_| Error::Setup)?;
    let second = root
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::DmaMappings, 1))
        .map_err(|_| Error::Setup)?;
    let vmo = VmoObject::try_new_writable(hyper::mm::PAGE_SIZE, &root).map_err(|_| Error::Setup)?;
    let backing = GuestMemoryBacking::try_from_vmo(&vmo).map_err(|_| Error::Setup)?;
    let lease = BackendMemoryLease::prepare(backing.clone(), &first).map_err(|_| Error::Setup)?;
    if first.usage().total(ResourceKind::DmaMappings) != 1
        || !matches!(
            BackendMemoryLease::prepare(backing.clone(), &first),
            Err(MemoryObjectError::Resource(_))
        )
    {
        return Err(Error::Quota);
    }
    let independent =
        BackendMemoryLease::prepare(backing.clone(), &second).map_err(|_| Error::Setup)?;
    let alias = Buffer::from(lease.clone());
    drop(lease);
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    if first.usage().total(ResourceKind::DmaMappings) != 1
        || second.usage().total(ResourceKind::DmaMappings) != 1
    {
        return Err(Error::Lifetime);
    }
    drop(alias);
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    if first.usage().total(ResourceKind::DmaMappings) != 0
        || second.usage().total(ResourceKind::DmaMappings) != 1
    {
        return Err(Error::Lifetime);
    }
    let reused = BackendMemoryLease::prepare(backing, &first).map_err(|_| Error::Setup)?;
    drop((reused, independent));
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    if first.usage().total(ResourceKind::DmaMappings) != 0
        || second.usage().total(ResourceKind::DmaMappings) != 0
    {
        return Err(Error::Lifetime);
    }
    // Metadata is available, but populating the first backing page must fail
    // after the DMA reservation. No logical mapping has been published yet.
    let no_pages = root
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::CommittedPages, 0))
        .map_err(|_| Error::Setup)?;
    let empty =
        VmoObject::try_new_writable(hyper::mm::PAGE_SIZE, &no_pages).map_err(|_| Error::Setup)?;
    let backing = GuestMemoryBacking::try_from_vmo(&empty).map_err(|_| Error::Setup)?;
    if BackendMemoryLease::prepare(backing, &first).is_ok()
        || first.usage().total(ResourceKind::DmaMappings) != 0
    {
        return Err(Error::Quota);
    }
    #[cfg(CONFIG_ARCH_AARCH64)]
    check_device_quota(&root)?;
    crate::pr_info!("HypeR test: DMA quota retained through aliases and final retirement");
    Ok(())
}

#[cfg(CONFIG_ARCH_AARCH64)]
fn check_device_quota(root: &ResourceDomain) -> Result<(), Error> {
    use crate::device::assigned::{DeviceAssignmentAuthority, PhysicalDevice};
    let denied = root
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::DeviceLeases, 0))
        .map_err(|_| Error::Setup)?;
    let allowed = root
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::DeviceLeases, 1))
        .map_err(|_| Error::Setup)?;
    // The authority is not itself a physical lease. A denied claim must fail
    // before discovery; the allowed but nonexistent device rolls reservation back.
    let authority = DeviceAssignmentAuthority::try_new(&denied).map_err(|_| Error::Setup)?;
    if denied.usage().total(ResourceKind::DeviceLeases) != 0
        || !matches!(
            PhysicalDevice::claim(usize::MAX, &denied),
            Err(crate::device::assigned::Error::Resource)
        )
        || PhysicalDevice::claim(usize::MAX, &allowed).is_ok()
        || allowed.usage().total(ResourceKind::DeviceLeases) != 0
    {
        return Err(Error::Quota);
    }
    drop(authority);
    Ok(())
}
