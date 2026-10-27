// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Lifetime of firmware-provided archive bytes. Namespace policy is in VFS;
//! this owner only retires its exclusively reserved physical pages.

use hyper::mm::boot::MAX_BOOT_RESERVATIONS;
use hyper::mm::ranges::RegionList;

pub(crate) struct Archive {
    bytes: &'static [u8],
    pages: RegionList<MAX_BOOT_RESERVATIONS>,
}

impl Archive {
    pub(super) fn new(bytes: &'static [u8], pages: RegionList<MAX_BOOT_RESERVATIONS>) -> Self {
        Self { bytes, pages }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn static_fixture(bytes: &'static [u8]) -> Self {
        Self::new(bytes, RegionList::new())
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        for &range in self.pages.as_slice() {
            // SAFETY: reserve_temporary excluded firmware, image, DTB and
            // partial-page overlaps before reserving the archive. All later
            // allocations excluded it. Every borrowed VFS node/parser pins
            // this owner, so its final drop ends the last possible read.
            let result = unsafe {
                crate::mm::allocator::policy::GLOBAL_ALLOCATOR.reclaim_reservation(range)
            };
            if let Err(error) = result {
                // Failed preflight leaves this range reserved. Never continue
                // by freeing pages whose ownership was not established.
                crate::pr_err!("HypeR: archive reservation retained: {error:?}");
            }
        }
    }
}
