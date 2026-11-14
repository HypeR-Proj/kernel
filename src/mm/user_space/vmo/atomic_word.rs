// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owned admission for a shared atomic word, independent of user mappings.

use super::{
    MappingAdmission, MemoryAccount, PageBackend, VmoError, VmoInner, VmoResult, WritableVmo,
    release_mapping_admission,
};
use hyper::mm::{FallibleArc, PAGE_SIZE};

/// Retains fixed VMO backing and excludes non-atomic direct access and snapshots.
///
/// This is an inline owner, not another heap allocation. The wait registration
/// charges its own metadata and pin to the caller; sparse page materialization
/// remains a charge to the VMO's existing backing account. A separate object
/// reference must retain the object identity used to key a shared wait.
pub(crate) struct SharedAtomicWord<Backend: PageBackend, Account: MemoryAccount> {
    inner: FallibleArc<VmoInner<Backend, Account>>,
    physical: u64,
}

impl<Backend: PageBackend, Account: MemoryAccount> SharedAtomicWord<Backend, Account> {
    /// Physical address of the admitted aligned word in coherent Normal memory.
    pub(crate) fn physical(&self) -> u64 {
        self.physical
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> Drop for SharedAtomicWord<Backend, Account> {
    fn drop(&mut self) {
        // The retained inner outlives admission release. Final backing/account
        // destruction happens after this method, outside the wait-engine lock.
        release_mapping_admission(&self.inner);
    }
}

impl<Backend: PageBackend, Account: MemoryAccount> WritableVmo<Backend, Account> {
    /// Checks geometry without touching memory, populating backing or admitting
    /// an access. Waking a shared wait must not require readable memory.
    pub(crate) fn validate_shared_atomic_offset(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, ()> {
        self.validate_atomic_width(offset, 4)
    }

    pub(crate) fn validate_shared_atomic_u64_offset(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, ()> {
        self.validate_atomic_width(offset, 8)
    }

    fn validate_atomic_width(&self, offset: u64, width: u64) -> VmoResult<Backend, Account, ()> {
        if !offset.is_multiple_of(width)
            || offset
                .checked_add(width)
                .is_none_or(|end| end > self.size())
        {
            return Err(VmoError::InvalidRange);
        }
        Ok(())
    }

    /// Admits atomic access alongside existing writable mappings. No ordinary
    /// Rust reference to remotely writable bytes is created or retained here.
    pub(crate) fn pin_shared_atomic_u32(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, SharedAtomicWord<Backend, Account>> {
        self.validate_shared_atomic_offset(offset)?;
        self.pin_atomic_word(offset)
    }

    pub(crate) fn pin_shared_atomic_u64(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, SharedAtomicWord<Backend, Account>> {
        self.validate_shared_atomic_u64_offset(offset)?;
        self.pin_atomic_word(offset)
    }

    fn pin_atomic_word(
        &self,
        offset: u64,
    ) -> VmoResult<Backend, Account, SharedAtomicWord<Backend, Account>> {
        let admission = MappingAdmission::acquire(&self.inner)?;
        let page_offset = offset / PAGE_SIZE * PAGE_SIZE;
        self.populate(page_offset, PAGE_SIZE)
            .map_err(|failure| failure.cause)?;
        let physical = self
            .resident_physical_page(page_offset)?
            .get()
            .checked_add(offset % PAGE_SIZE)
            .ok_or(VmoError::SizeOverflow)?;
        // Writable VMOs never replace or decommit resident pages. Retaining
        // their inner is sufficient to pin this physical word after unmap and
        // handle closure. The admission moves exactly once into the owner.
        let word = SharedAtomicWord {
            inner: self.inner.clone(),
            physical,
        };
        admission.commit();
        Ok(word)
    }
}
