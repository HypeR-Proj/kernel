// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One independently reclaimable physical page, with an initialized prefix.

#[cfg(test)]
use alloc::vec::Vec;

use super::CacheError;

const PAGE_BYTES: usize = hyper::mm::PAGE_SIZE as usize;

pub(super) struct PageBytes {
    #[cfg(not(test))]
    _page: crate::mm::page_block::PageBlock,
    #[cfg(not(test))]
    address: usize,
    #[cfg(not(test))]
    length: usize,
    #[cfg(test)]
    bytes: Vec<u8>,
}

impl PageBytes {
    pub(super) fn try_zeroed(length: usize) -> Result<Self, CacheError> {
        if length > PAGE_BYTES {
            return Err(CacheError::InvalidCapacity);
        }
        let mut storage = Self::try_copy(&[])?;
        storage.prepare_fill(length);
        Ok(storage)
    }

    /// Initialize newly exposed bytes before lending a mutable slice to I/O.
    /// Existing bytes need not be cleared: publication requires a complete
    /// successful fill, and a failed load destroys this exclusive owner.
    pub(super) fn prepare_fill(&mut self, length: usize) {
        if length > PAGE_BYTES {
            super::cache_invariant_violation();
        }
        #[cfg(not(test))]
        {
            if length > self.length {
                // SAFETY: This exclusive owner retains a full physical page;
                // the newly initialized suffix lies within that allocation.
                unsafe {
                    core::ptr::with_exposed_provenance_mut::<u8>(self.address)
                        .add(self.length)
                        .write_bytes(0, length - self.length);
                }
            }
            self.length = length;
        }
        #[cfg(test)]
        self.bytes.resize(length, 0);
    }

    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        #[cfg(not(test))]
        {
            // SAFETY: prepare_fill initialized this prefix; exclusive ownership
            // excludes cache readers and all user or device mapping aliases.
            unsafe {
                core::slice::from_raw_parts_mut(
                    core::ptr::with_exposed_provenance_mut::<u8>(self.address),
                    self.length,
                )
            }
        }
        #[cfg(test)]
        &mut self.bytes
    }

    pub(super) fn truncate(&mut self, length: usize) {
        if length > self.as_slice().len() {
            super::cache_invariant_violation();
        }
        #[cfg(not(test))]
        {
            self.length = length;
        }
        #[cfg(test)]
        self.bytes.truncate(length);
    }

    pub(super) fn try_copy(bytes: &[u8]) -> Result<Self, CacheError> {
        Self::try_copy_parts(&[bytes])
    }

    /// Assemble a bounded payload without a page-sized temporary on the
    /// caller's stack. All component lengths are validated before allocation.
    pub(super) fn try_copy_parts(parts: &[&[u8]]) -> Result<Self, CacheError> {
        parts_length(parts).ok_or(CacheError::InvalidCapacity)?;
        #[cfg(not(test))]
        {
            let page =
                crate::mm::cache_memory::allocate_page().map_err(|_| CacheError::Allocation)?;
            let address = crate::mm::memory::linear_address(page.physical().get())
                .ok_or(CacheError::Allocation)?;
            let mut storage = Self {
                _page: page,
                address,
                length: 0,
            };
            storage.replace_parts(parts);
            Ok(storage)
        }
        #[cfg(test)]
        {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(PAGE_BYTES)
                .map_err(|_| CacheError::Allocation)?;
            for part in parts {
                owned.extend_from_slice(part);
            }
            Ok(Self { bytes: owned })
        }
    }

    pub(super) fn replace(&mut self, bytes: &[u8]) {
        self.replace_parts(&[bytes]);
    }

    pub(super) fn replace_parts(&mut self, parts: &[&[u8]]) {
        let Some(length) = parts_length(parts) else {
            super::cache_invariant_violation();
        };
        #[cfg(not(test))]
        {
            let mut offset = 0;
            for part in parts {
                // SAFETY: This owner retains one order-0 block in the permanent
                // writable RAM map. Validated lengths keep every destination
                // inside it. Exclusive access prevents readers and aliases of
                // this page; only the fully initialized prefix is published.
                unsafe {
                    core::ptr::with_exposed_provenance_mut::<u8>(self.address)
                        .add(offset)
                        .copy_from_nonoverlapping(part.as_ptr(), part.len());
                }
                offset += part.len();
            }
            self.length = length;
        }
        #[cfg(test)]
        {
            self.bytes.clear();
            for part in parts {
                self.bytes.extend_from_slice(part);
            }
            debug_assert_eq!(self.bytes.len(), length);
        }
    }

    #[cfg(not(test))]
    pub(super) fn physical_address(&self) -> hyper::mm::PhysicalAddress {
        self._page.physical()
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        #[cfg(not(test))]
        {
            // SAFETY: The retained block outlives this borrow, and exactly this
            // prefix was initialized before publication. Mutation needs &mut.
            unsafe {
                core::slice::from_raw_parts(
                    core::ptr::with_exposed_provenance::<u8>(self.address),
                    self.length,
                )
            }
        }
        #[cfg(test)]
        &self.bytes
    }
}

fn parts_length(parts: &[&[u8]]) -> Option<usize> {
    parts.iter().try_fold(0_usize, |length, part| {
        length
            .checked_add(part.len())
            .filter(|length| *length <= PAGE_BYTES)
    })
}
