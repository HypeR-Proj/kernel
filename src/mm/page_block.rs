// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RAII ownership for physically contiguous kernel page allocations.

use crate::mm::allocator::heap::PageOwner;
use {crate::mm::allocator::BuddyError, hyper::mm::PhysicalAddress};

/// A live buddy allocation returned to the kernel by the global page allocator.
///
/// This type is intentionally kernel-owned: allocation policy and the fatal
/// response to allocator corruption do not belong in the reusable buddy core.
pub struct PageBlock {
    physical: PhysicalAddress,
    order: usize,
    owner: PageOwner,
}

impl PageBlock {
    pub fn allocate(order: usize) -> Result<Self, BuddyError> {
        Self::allocate_for(order, PageOwner::Kernel)
    }

    pub fn allocate_for(order: usize, owner: PageOwner) -> Result<Self, BuddyError> {
        let physical =
            crate::mm::allocator::policy::GLOBAL_ALLOCATOR.allocate_pages_for(order, owner)?;
        Ok(Self {
            physical,
            order,
            owner,
        })
    }

    pub(super) fn allocate_cache(min_free_pages: usize) -> Result<Self, BuddyError> {
        let physical = crate::mm::allocator::policy::GLOBAL_ALLOCATOR
            .allocate_pages_above_reserve(0, PageOwner::FileCache, min_free_pages)?;
        Ok(Self {
            physical,
            order: 0,
            owner: PageOwner::FileCache,
        })
    }

    pub const fn physical(&self) -> PhysicalAddress {
        self.physical
    }

    pub const fn order(&self) -> usize {
        self.order
    }

    pub const fn owner(&self) -> PageOwner {
        self.owner
    }
}

impl Drop for PageBlock {
    fn drop(&mut self) {
        // SAFETY: PageBlock is the unique owner of this exact buddy block and
        // relinquishes it exactly once from Drop.
        match unsafe {
            crate::mm::allocator::policy::GLOBAL_ALLOCATOR.deallocate_pages_for(
                self.physical,
                self.order,
                self.owner,
            )
        } {
            Ok(()) => {}
            Err(error) => {
                // PageBlock exists only after permanent memory and CPU identity
                // are available. Crash entry is allocation-free and tolerates
                // arbitrary outer locks, including before full crash readiness.
                crate::diagnostics::crash::fatal(format_args!(
                    "PageBlock buddy deallocation invariant failed: {error:?}"
                ));
            }
        }
    }
}
