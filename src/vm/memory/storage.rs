// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded backing metadata and stage-2 table-page ownership.

use core::alloc::Layout;
use core::ptr::write_bytes;

use crate::mm::allocator::heap::PageOwner;
use alloc::vec::Vec;
use hyper::mm::{PAGE_SIZE, PhysicalAddress};

use super::Error;
use super::backing::Layout as SharedGuestMemory;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use crate::hal::vm::Stage2AddressSpace;
use crate::mm::page_block::PageBlock;
use crate::mm::reclaim::Target;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[path = "../../../tests/kernel/vm_memory_storage.rs"]
mod tests;

pub(super) enum GuestMemoryBacking {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    KernelOwned(Vec<Option<PageBlock>>),
    SharedVmo(alloc::boxed::Box<SharedGuestMemory>),
}

/// Frozen membership of the resident pages covered by one batched instruction
/// publication.
///
/// The compact bitmap bounds transient metadata for large guests. Every
/// selected address is validated before entering the HAL; backing ownership
/// then keeps its physical page and permanent linear mapping stable until the
/// guest address space retires.
pub(crate) struct ResidentInstructionSnapshot {
    pub(super) pages: FixedBitmap,
    pub(super) _metadata_charge: CommittedCharge,
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl ResidentInstructionSnapshot {
    pub(crate) fn retained_metadata_bytes_for_test(&self) -> u64 {
        self.pages.retained_bytes() as u64
    }
}

pub(super) use hyper::collections::fixed_bitmap::FixedBitmap;
impl From<hyper::collections::fixed_bitmap::Error> for Error {
    fn from(error: hyper::collections::fixed_bitmap::Error) -> Self {
        match error {
            hyper::collections::fixed_bitmap::Error::Allocation => Self::MetadataAllocation,
            hyper::collections::fixed_bitmap::Error::InvalidRange => Self::InvalidRange,
        }
    }
}

pub(super) struct AddressSpaceMetadataLayout {
    pub(super) page_count: usize,
    pub(super) table_capacity: usize,
    pub(super) bytes: u64,
}

pub(super) struct AdmittedAddressSpaceMetadata {
    pub(super) page_count: usize,
    pub(super) table_capacity: usize,
    pub(super) charge: CommittedCharge,
}
pub(super) struct TablePage {
    page: PageBlock,
    detached: bool,
}

pub(super) struct Stage2PagePool {
    initial_capacity: usize,
    pub(super) pages: Vec<TablePage>,
    domain: ResourceDomain,
    charge: Option<CommittedCharge>,
    error: Option<Error>,
}

/// Only a failed, unpublished capacity reservation may request reclaim.
/// The caller must release the address-space spinlock before servicing it.
pub(crate) struct LivePreparationError {
    pub(super) error: Error,
    pub(super) reclaim: Option<Target>,
}

impl From<Error> for LivePreparationError {
    fn from(error: Error) -> Self {
        Self {
            error,
            reclaim: None,
        }
    }
}

impl Stage2PagePool {
    pub(super) fn with_capacity(capacity: usize, domain: &ResourceDomain) -> Result<Self, Error> {
        let pages = try_exact_capacity_vec(capacity)?;
        Ok(Self {
            initial_capacity: capacity,
            pages,
            domain: domain.clone(),
            charge: None,
            error: None,
        })
    }

    pub(super) fn live_capacity_ready(&self, live_capacity: usize) -> Result<bool, Error> {
        Ok(self.live_capacity_target(live_capacity)? <= self.pages.capacity())
    }
    fn live_capacity_target(&self, live_capacity: usize) -> Result<usize, Error> {
        self.pages
            .len()
            .checked_add(self.initial_capacity)
            .and_then(|value| value.checked_add(live_capacity))
            .ok_or(Error::MetadataAllocation)
    }

    pub(super) fn reserve_live(
        &mut self,
        live_capacity: usize,
    ) -> Result<(), LivePreparationError> {
        let target = self.live_capacity_target(live_capacity)?;
        let additional_capacity = target.saturating_sub(self.pages.capacity());
        if additional_capacity == 0 {
            return Ok(());
        }
        let layout = Layout::array::<TablePage>(target).map_err(|_| Error::MetadataAllocation)?;
        let allocation_pages = crate::mm::cache_memory::allocation_page_bound(layout)
            .ok_or(Error::MetadataAllocation)?;
        let bytes = additional_capacity
            .checked_mul(core::mem::size_of::<TablePage>())
            .ok_or(Error::MetadataAllocation)?;
        let reservation = self
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
            .map_err(|error| LivePreparationError {
                error: Error::Resource(error),
                reclaim: match error {
                    ResourceError::LimitExceeded {
                        domain,
                        resource: ResourceKind::KernelMemoryBytes,
                        ..
                    } => Some(Target::Domain(domain)),
                    _ => None,
                },
            })?;
        self.pages
            .try_reserve_exact(target - self.pages.len())
            .map_err(|_| LivePreparationError {
                error: Error::MetadataAllocation,
                reclaim: Some(Target::PhysicalOrder(
                    allocation_pages.trailing_zeros() as usize
                )),
            })?;
        accumulate_charge(&mut self.charge, reservation.commit());
        Ok(())
    }

    pub(super) fn allocate_zeroed(
        &mut self,
        pages: usize,
        alignment_pages: usize,
    ) -> Option<PhysicalAddress> {
        if pages == 0
            || !pages.is_power_of_two()
            || alignment_pages != pages
            || self.pages.len() == self.pages.capacity()
        {
            return None;
        }
        let byte_count = pages.checked_mul(PAGE_SIZE as usize)?;
        let page_count = u64::try_from(pages).ok()?;
        let byte_count_u64 = u64::try_from(byte_count).ok()?;
        let reservation = match self.domain.reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelMemoryBytes, byte_count_u64)
                .with(ResourceKind::CommittedPages, page_count)
                .with(ResourceKind::PinnedPages, page_count),
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                self.error = Some(Error::Resource(error));
                return None;
            }
        };
        let order = pages.trailing_zeros() as usize;
        let page = match PageBlock::allocate_for(order, PageOwner::PageTable) {
            Ok(page) => page,
            Err(error) => {
                self.error = Some(Error::Allocation(error));
                return None;
            }
        };
        let physical = page.physical();
        let virtual_address = match linear_address(physical) {
            Ok(address) => address,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        // SAFETY: This new table page is exclusive and permanently mapped.
        unsafe { write_bytes(virtual_address as *mut u8, 0, byte_count) };
        self.pages.push(TablePage {
            page,
            detached: false,
        });
        accumulate_charge(&mut self.charge, reservation.commit());
        Some(physical)
    }

    /// Withdraws a table from future walks; physical ownership remains here.
    pub(super) fn detach(&mut self, physical: PhysicalAddress) {
        let Some(owner) = self
            .pages
            .iter_mut()
            .find(|owner| owner.page.physical() == physical)
        else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: detached stage-2 table has no owner"
            ));
        };
        if owner.detached {
            crate::diagnostics::crash::fatal(format_args!("HypeR: stage-2 table detached twice"));
        }
        owner.detached = true;
    }

    /// # Safety
    /// All detached descriptors must have passed an acknowledged all-CPU
    /// invalidation. The returned storage and charge are dropped outside locks.
    pub(super) unsafe fn take_retired(&mut self) -> Option<(PageBlock, CommittedCharge)> {
        let index = self.pages.iter().position(|owner| owner.detached)?;
        let owner = self.pages.swap_remove(index);
        let pages = 1u64 << owner.page.order();
        let Some(charge) = self.charge.as_mut() else {
            crate::diagnostics::crash::fatal(format_args!("HypeR: stage-2 table charge missing"));
        };
        let charge = charge.split_off(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelMemoryBytes, pages * PAGE_SIZE)
                .with(ResourceKind::CommittedPages, pages)
                .with(ResourceKind::PinnedPages, pages),
        );
        Some((owner.page, charge))
    }

    /// Trim spare owner slots after a live grant retires. Failure preserves the
    /// current pool; it must never turn completed DMA retirement into an error.
    /// The returned empty allocation and charge are destroyed outside the lock.
    pub(super) fn compact_live(
        &mut self,
        live_capacity: usize,
    ) -> Option<(Vec<TablePage>, CommittedCharge)> {
        let target = self.live_capacity_target(live_capacity).ok()?;
        let old_capacity = self.pages.capacity();
        if target >= old_capacity {
            return None;
        }
        // Allocation under the address-space lock is nonblocking and fallible;
        // no scheduled reclaim may run here. Preserve all fault reservations.
        let mut replacement = try_exact_capacity_vec(target).ok()?;
        replacement.append(&mut self.pages);
        core::mem::swap(&mut self.pages, &mut replacement);
        let bytes = (old_capacity - target) * core::mem::size_of::<TablePage>();
        let Some(charge) = self.charge.as_mut() else {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: stage-2 metadata charge missing"
            ));
        };
        let charge = charge
            .split_off(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64));
        Some((replacement, charge))
    }

    pub(super) fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }
}

pub(super) fn accumulate_charge(owner: &mut Option<CommittedCharge>, charge: CommittedCharge) {
    match owner {
        Some(owner) => owner.absorb_pre_admitted(charge),
        None => *owner = Some(charge),
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) fn address_space_metadata_layout(
    ipa_base: u64,
    size: u64,
    page_count: usize,
    backing_owner_bytes: usize,
) -> Result<AddressSpaceMetadataLayout, Error> {
    let table_capacity = Stage2AddressSpace::required_table_pages(ipa_base, size)?;
    metadata_for_capacity(page_count, table_capacity, backing_owner_bytes)
}

pub(super) fn metadata_for_capacity(
    page_count: usize,
    table_capacity: usize,
    backing_owner_bytes: usize,
) -> Result<AddressSpaceMetadataLayout, Error> {
    let bitmap_bytes = bitmap_storage_bytes(page_count)
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or(Error::MetadataAllocation)?;
    let table_owner_bytes = table_capacity
        .checked_mul(core::mem::size_of::<TablePage>())
        .ok_or(Error::MetadataAllocation)?;
    let bytes = bitmap_bytes
        .checked_add(table_owner_bytes)
        .and_then(|bytes| bytes.checked_add(backing_owner_bytes))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::MetadataAllocation)?;
    Ok(AddressSpaceMetadataLayout {
        page_count,
        table_capacity,
        bytes,
    })
}

pub(super) fn admit_metadata(
    domain: &ResourceDomain,
    layout: AddressSpaceMetadataLayout,
) -> Result<AdmittedAddressSpaceMetadata, Error> {
    let charge = domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, layout.bytes))?
        .commit();
    Ok(AdmittedAddressSpaceMetadata {
        page_count: layout.page_count,
        table_capacity: layout.table_capacity,
        charge,
    })
}

/// Returns the allocator payload required by a [`FixedBitmap`].
///
/// The final partial machine word remains fully retained and charged.
pub(super) fn bitmap_storage_bytes(bit_count: usize) -> Option<usize> {
    hyper::collections::fixed_bitmap::storage_bytes(bit_count)
}

/// Allocates a Vec whose retained allocator request is exactly the admitted
/// element payload.
///
/// `HypeR` charges requested heap bytes rather than the allocator's internal
/// slab/page rounding. Starting from an empty Vec and using `reserve_exact`
/// should retain precisely this capacity with the pinned Rust toolchain. The
/// explicit check turns any future library growth-policy change into a
/// failure before the storage is published under an undersized charge.
pub(super) fn try_exact_capacity_vec<T>(capacity: usize) -> Result<Vec<T>, Error> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| Error::MetadataAllocation)?;
    if values.capacity() != capacity {
        return Err(Error::MetadataAllocation);
    }
    Ok(values)
}

pub(super) fn validate_region(ipa_base: u64, size: u64) -> Result<usize, Error> {
    if size == 0 || ipa_base & (PAGE_SIZE - 1) != 0 || size & (PAGE_SIZE - 1) != 0 {
        return Err(Error::InvalidRange);
    }
    ipa_base.checked_add(size).ok_or(Error::AddressOverflow)?;
    usize::try_from(size / PAGE_SIZE).map_err(|_| Error::AddressOverflow)
}

pub(super) fn linear_address(physical: PhysicalAddress) -> Result<usize, Error> {
    crate::mm::memory::linear_address(physical.get()).ok_or(Error::InvalidRange)
}
