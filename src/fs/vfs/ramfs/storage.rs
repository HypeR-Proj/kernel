// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RAMFS payload pages are authoritative storage, not reclaimable cache pages.

use crate::fs::primitives::file_data::{PAGE_BYTES, Page, Storage, StorageBudget};
use crate::mm::page_block::PageBlock;
use hyper::mm::AllocationError;

pub(super) struct DataPage {
    _page: PageBlock,
    address: usize,
}

impl Storage for super::DataBudget<'_> {
    type Page = DataPage;

    fn reserve_page(&self) -> Result<super::DataCharge, crate::accounting::ResourceError> {
        let mut charge = self.reserve(PAGE_BYTES)?;
        charge.bytes = PAGE_BYTES as u64;
        super::DATA_CAPACITY_BYTES.fetch_add(charge.bytes, core::sync::atomic::Ordering::Relaxed);
        Ok(charge)
    }

    fn allocate_zeroed_page(&self) -> Result<DataPage, AllocationError> {
        let page = PageBlock::allocate(0).map_err(|_| AllocationError)?;
        let address =
            crate::mm::memory::linear_address(page.physical().get()).ok_or(AllocationError)?;
        // SAFETY: This unique order-0 owner retains a complete page in the
        // permanent writable RAM map. No slice, mapping or DMA alias exists.
        // Initialize the whole page before exposing any safe byte references.
        unsafe {
            core::ptr::with_exposed_provenance_mut::<u8>(address).write_bytes(0, PAGE_BYTES);
        }
        Ok(DataPage {
            _page: page,
            address,
        })
    }
}

impl Page for DataPage {
    fn bytes(&self) -> &[u8; PAGE_BYTES] {
        // SAFETY: The initialized order-0 allocation outlives this borrow.
        // The owner is private; mutation requires its exclusive mutable borrow.
        unsafe { &*core::ptr::with_exposed_provenance::<[u8; PAGE_BYTES]>(self.address) }
    }

    fn bytes_mut(&mut self) -> &mut [u8; PAGE_BYTES] {
        // SAFETY: Exclusive access to this owner excludes all byte borrows.
        // The initialized allocation remains live and has no external aliases.
        unsafe { &mut *core::ptr::with_exposed_provenance_mut::<[u8; PAGE_BYTES]>(self.address) }
    }
}
