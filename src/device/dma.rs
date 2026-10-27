// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-only ownership of resident memory shared with a physical backend.

use alloc::vec::Vec;
use hyper::mm::PAGE_SIZE;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind, crate::authority::Rights,
    crate::mm::user_space::GuestMemoryBacking, crate::mm::user_space::MemoryObjectError,
    crate::object::KernelObject, crate::object::KernelRef, crate::object::KernelService,
    crate::object::ObjectKind, crate::object::TransferClass, crate::object::private,
};

/// The lifetime shared by CPU mappings and IOMMU mappings. Closing a source
/// handle, removing a CPU mapping, or dropping a caller's reference cannot
/// release pages still held by a DMA domain. No hardware operation runs in Drop.
pub(crate) struct BackendMemoryLease {
    pages: Vec<u64>,
    _backing: GuestMemoryBacking,
    _charge: CommittedCharge,
}

pub(crate) type Lease = KernelRef<BackendMemoryLease, KernelService>;

impl BackendMemoryLease {
    /// Populate and snapshot once, in sleeping context, before any device sees
    /// the buffer. The exclusive hardware lease prevents frame substitution and
    /// incompatible Native writers throughout every derived owner's lifetime.
    pub(crate) fn prepare(
        backing: GuestMemoryBacking,
        domain: &ResourceDomain,
    ) -> Result<Lease, MemoryObjectError> {
        let size = backing.size();
        Self::prepare_range(backing, 0, size, domain)
    }

    pub(crate) fn prepare_range(
        backing: GuestMemoryBacking,
        start: u64,
        length: u64,
        domain: &ResourceDomain,
    ) -> Result<Lease, MemoryObjectError> {
        if !start.is_multiple_of(PAGE_SIZE)
            || start
                .checked_add(length)
                .is_none_or(|end| end > backing.size())
        {
            return Err(MemoryObjectError::AllocationSize);
        }
        let count =
            usize::try_from(length / PAGE_SIZE).map_err(|_| MemoryObjectError::AllocationSize)?;
        if count == 0 || !length.is_multiple_of(PAGE_SIZE) {
            return Err(MemoryObjectError::AllocationSize);
        }
        let bytes = count
            .checked_mul(core::mem::size_of::<u64>())
            .and_then(|bytes| crate::object::object_allocation_size::<Self>()?.checked_add(bytes))
            .ok_or(MemoryObjectError::AllocationSize)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, bytes as u64),
            )?
            .commit();
        let mut pages = Vec::new();
        crate::mm::reclaim::reserve_exact(&mut pages, count)
            .map_err(|_| MemoryObjectError::AllocationSize)?;
        for index in 0..count {
            let offset = start + index as u64 * PAGE_SIZE;
            backing.populate_page(offset)?;
            pages.push(backing.physical_page(offset)?.get());
        }
        Lease::try_new_service(Self {
            pages,
            _backing: backing,
            _charge: charge,
        })
        .map_err(MemoryObjectError::Object)
    }

    pub(crate) fn length(&self) -> u64 {
        self.pages.len() as u64 * PAGE_SIZE
    }

    pub(crate) fn physical_page(&self, offset: u64) -> u64 {
        // Only validated page offsets from retained mappings reach this path.
        if !offset.is_multiple_of(PAGE_SIZE) || offset >= self.length() {
            hyper::debug::invariant_failure("DMA lease page outside retained buffer");
        }
        self.pages[(offset / PAGE_SIZE) as usize]
    }
}

/// A checked slice retains the complete lease while exposing only the pages
/// authorized by one contiguous IOVA extent to the translation driver.
#[cfg_attr(
    all(feature = "kernel-test", feature = "io-dma-revoke-test"),
    derive(Clone)
)]
pub(crate) struct Buffer {
    lease: Lease,
    offset: u64,
    length: u64,
}
impl Buffer {
    pub(crate) fn slice(
        lease: Lease,
        offset: u64,
        length: u64,
    ) -> Result<Self, hyper_drivers::iommu::smmuv3::Error> {
        if length == 0
            || !offset.is_multiple_of(PAGE_SIZE)
            || !length.is_multiple_of(PAGE_SIZE)
            || offset
                .checked_add(length)
                .is_none_or(|end| end > lease.object().length())
        {
            return Err(hyper_drivers::iommu::smmuv3::Error::Address);
        }
        Ok(Self {
            lease,
            offset,
            length,
        })
    }
}
impl From<Lease> for Buffer {
    fn from(lease: Lease) -> Self {
        let length = lease.object().length();
        Self {
            lease,
            offset: 0,
            length,
        }
    }
}
// SAFETY: The checked subrange owns a counted stable backing lease. The driver
// only asks for aligned page offsets below length and never receives CPU bytes.
unsafe impl hyper_drivers::iommu::smmuv3::DmaBuffer for Buffer {
    fn length(&self) -> u64 {
        self.length
    }
    fn physical_page(&self, offset: u64) -> u64 {
        if offset >= self.length {
            hyper::debug::invariant_failure("DMA buffer page out of bounds");
        }
        self.lease.object().physical_page(self.offset + offset)
    }
}

impl private::Sealed for BackendMemoryLease {}
impl KernelObject for BackendMemoryLease {
    const KIND: ObjectKind = ObjectKind::BACKEND_MEMORY_LEASE;
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;
    const SUPPORTED_RIGHTS: Rights = Rights::NONE;
}

// SAFETY: Construction snapshots only resident pages of a stable VMO protected
// by its exclusive hardware lease. Every KernelRef retains that lease and all
// pages; no borrowed CPU bytes are exposed by this interface.
unsafe impl hyper_drivers::iommu::smmuv3::DmaBuffer for Lease {
    fn length(&self) -> u64 {
        self.object().length()
    }
    fn physical_page(&self, offset: u64) -> u64 {
        self.object().physical_page(offset)
    }
}
