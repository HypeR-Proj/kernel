// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-write replacement in the common file cache. Dirty owners keep both
//! bytes and their canonical key resident until the filesystem accepts them.

use super::{
    budget::{Budget, OwnedPage},
    *,
};
use core::sync::atomic::Ordering;
use hyper::mm::FallibleArc;

pub(crate) struct DirtyPage<Page> {
    pub(super) page: MappedPage<Page>,
    pub(super) _admission: DirtyAdmission,
}
impl<Page> DirtyPage<Page> {
    pub(crate) fn value(&self) -> &Page {
        self.page.value()
    }
}
pub(super) struct DirtyAdmission(FallibleArc<Budget>);
impl Drop for DirtyAdmission {
    fn drop(&mut self) {
        if self.0.dirty.fetch_sub(1, Ordering::AcqRel) == 0 {
            cache_invariant_violation();
        }
        notify_worker();
    }
}

impl<Owner: Clone> FileDataCache<FilePage<Owner>> {
    /// The file content gate serializes writers and new read/mapping lookups.
    /// Existing readers retain an immutable old version. No backend operation
    /// or payload destruction runs under the cache lock. None means admission
    /// pressure; the caller must drain dirty data before retry/fallback.
    pub(crate) fn write_page<E: From<CacheError>>(
        &self,
        key: CacheKey,
        length: usize,
        owner: &Owner,
        fill: impl FnOnce(Option<&FilePage<Owner>>, &mut FilePage<Owner>) -> Result<(), E>,
    ) -> Result<Option<DirtyPage<FilePage<Owner>>>, E> {
        // Allocator observation must precede the cache lock. Actual payload
        // admission is checked again atomically by the allocator below.
        let allowed = self.admission.load(Ordering::Acquire) && memory::admission_allowed();
        let admission = self.state.with(|control| {
            if control.table.is_none()
                || control.maintenance_running
                || control.maintenance_pending
                || control.admission_pauses != 0
                || !allowed
            {
                return None;
            }
            self.budget.dirty.fetch_add(1, Ordering::AcqRel);
            Some(DirtyAdmission(self.budget.clone()))
        });
        let Some(admission) = admission else {
            return Ok(None);
        };
        if let Some(old) = self.lookup_mapping(key)? {
            let Some(permit) = Budget::reserve(&self.budget) else {
                return Ok(None);
            };
            let layout = Layout::from_size_align(
                FallibleArc::<OwnedPage<FilePage<Owner>>>::allocation_size(),
                core::mem::align_of::<OwnedPage<FilePage<Owner>>>(),
            )
            .map_err(|_| CacheError::InvalidCapacity)?;
            let (reservation, _) = memory::reserve(&[layout])?;
            let mut value = FilePage::try_empty(length, owner.clone())?;
            fill(Some(old.value()), &mut value)?;
            let shared = memory::shared(OwnedPage::new(value, permit))?;
            shared.retain_mapping();
            let page = MappedPage {
                inner: shared.clone(),
            };
            let retired = self.state.with(|control| -> Result<_, CacheError> {
                let cache = control.table.as_mut().ok_or(CacheError::StaleLoad)?;
                let slot = cache.model.lookup(key)?.ok_or(CacheError::StaleLoad)?;
                Ok(cache.pages[slot].replace(shared))
            })?;
            drop(retired);
            drop(reservation);
            Ok(Some(DirtyPage {
                page,
                _admission: admission,
            }))
        } else {
            let CacheAccess::Load(load) = self.access(key)? else {
                return Ok(None);
            };
            let mut pending = load.prepare_fill(
                || FilePage::try_empty(length, owner.clone()),
                |page| page.prepare_fill(length, owner.clone()),
            )?;
            fill(None, pending.value_mut())?;
            Ok(Some(pending.publish_dirty(admission)?))
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/host/src/cases/file_cache_write.rs"]
mod tests;
