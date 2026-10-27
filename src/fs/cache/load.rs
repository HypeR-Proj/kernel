// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Linear fill ownership, exclusive payload reuse, and cache publication.

use core::alloc::Layout;
#[cfg(test)]
use core::sync::atomic::Ordering;

use hyper::mm::{FallibleArc, UniqueFallibleArc};

use super::budget::{OwnedPage, Permit};
use super::{CacheError, CachedPage, FileDataCache, cache_invariant_violation, state};

pub(super) enum FillStorage<Page> {
    Fresh(Permit),
    Reused(UniqueFallibleArc<OwnedPage<Page>>),
}

pub(crate) enum Refill {
    /// The existing allocation now holds the complete replacement payload.
    Ready,
    /// Destroy the old payload before calling the fresh allocation factory.
    #[cfg_attr(
        not(test),
        cfg_attr(
            not(all(feature = "kernel-test", feature = "kernel-self-test")),
            expect(
                dead_code,
                reason = "generic payloads may need recreation; physical file pages always fit"
            )
        )
    )]
    Recreate,
}

#[must_use = "fill and publish the page or let the reservation roll back"]
pub(crate) struct LoadReservation<'cache, Page> {
    cache: &'cache FileDataCache<Page>,
    token: state::LoadToken,
    storage: Option<FillStorage<Page>>,
    active: bool,
}

impl<'cache, Page> LoadReservation<'cache, Page> {
    pub(super) fn new(
        cache: &'cache FileDataCache<Page>,
        token: state::LoadToken,
        storage: FillStorage<Page>,
    ) -> Self {
        Self {
            cache,
            token,
            storage: Some(storage),
            active: true,
        }
    }

    /// Create a payload only after admission, or refill a uniquely owned victim
    /// without allocation. `refill` must check capacity before changing bytes;
    /// it must not allocate another payload or grow an existing allocation.
    /// Recreate drops the old payload before invoking `create` with its permit
    /// retained. Every failure destroys data before releasing that permit.
    pub(crate) fn fill_and_publish(
        mut self,
        create: impl FnOnce() -> Result<Page, CacheError>,
        refill: impl FnOnce(&mut Page) -> Refill,
    ) -> Result<CachedPage<Page>, CacheError> {
        let Some(storage) = self.storage.take() else {
            cache_invariant_violation();
        };
        let shared = match storage {
            FillStorage::Fresh(permit) => self.create(permit, create)?,
            FillStorage::Reused(mut page) => match refill(&mut page.value) {
                Refill::Ready => {
                    #[cfg(test)]
                    self.cache.payload_reuses.fetch_add(1, Ordering::Relaxed);
                    page.into_shared()
                }
                Refill::Recreate => {
                    let (old, permit) = page.into_inner().into_parts();
                    drop(old);
                    self.create(permit, create)?
                }
            },
        };
        #[cfg(test)]
        if self.cache.fail_publication.swap(false, Ordering::Relaxed) {
            return Err(CacheError::Allocation);
        }
        let result = self.cache.state.with(|control| {
            let Some(cache) = control.table.as_mut() else {
                return Err(CacheError::StaleLoad);
            };
            cache.model.validate(self.token)?;
            if !cache
                .pages
                .get(self.token.slot())
                .is_some_and(Option::is_none)
            {
                cache_invariant_violation();
            }
            if cache.model.publish(self.token).is_err() {
                cache_invariant_violation();
            }
            let Some(slot) = cache.pages.get_mut(self.token.slot()) else {
                cache_invariant_violation();
            };
            *slot = Some(shared.clone());
            Ok::<(), CacheError>(())
        });
        // A failed validation means another generation owns this slot. Its
        // index entry must not be changed by the old reservation's destructor.
        self.active = false;
        self.cache.load_finished();
        result?;
        Ok(CachedPage { inner: shared })
    }

    fn create(
        &self,
        permit: Permit,
        create: impl FnOnce() -> Result<Page, CacheError>,
    ) -> Result<FallibleArc<OwnedPage<Page>>, CacheError> {
        let layout = Layout::from_size_align(
            FallibleArc::<OwnedPage<Page>>::allocation_size(),
            core::mem::align_of::<OwnedPage<Page>>(),
        )
        .map_err(|_| CacheError::InvalidCapacity)?;
        let (reservation, _) = super::memory::reserve(&[layout])?;
        let page = create()?;
        #[cfg(test)]
        self.cache.payload_creations.fetch_add(1, Ordering::Relaxed);
        let shared = super::memory::shared(OwnedPage::new(page, permit))?;
        drop(reservation);
        Ok(shared)
    }

    #[cfg(test)]
    pub(crate) fn abort(mut self) -> Result<(), CacheError> {
        let result = self.cache.state.with(|control| {
            control
                .table
                .as_mut()
                .ok_or(CacheError::StaleLoad)?
                .model
                .abort(self.token)
                .map_err(Into::into)
        });
        self.active = false;
        self.cache.load_finished();
        result
    }

    #[cfg(test)]
    pub(crate) fn invalidate_slot_for_test(&self) {
        self.cache.state.with(|control| {
            if let Some(cache) = control.table.as_mut() {
                cache.model.invalidate_for_test(self.token.slot());
            }
        });
    }
}

impl<Page> Drop for LoadReservation<'_, Page> {
    fn drop(&mut self) {
        if self.active {
            // The callback releases the cache lock before storage destruction.
            // A stale token cannot remove a replacement generation's entry.
            self.cache.state.with(|control| {
                if let Some(cache) = control.table.as_mut() {
                    let _ = cache.model.abort(self.token);
                }
            });
            self.cache.load_finished();
        }
    }
}
