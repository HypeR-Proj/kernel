// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded kernel-owned cache for immutable file data and filesystem metadata.
//!
//! The cache owns only clean, immutable payloads. A miss reserves one slot
//! under the cache lock, but the caller allocates and fills its payload after
//! that lock has been released. Publication consumes a generation-tagged
//! reservation, so a stale completion cannot populate a recycled slot.
//!
//! The system service owns a pressure-managed payload budget, independent of
//! the first reader or mount. Every loader reserves capacity before allocating, and the
//! immutable payload keeps that permit even after eviction until its last
//! reader releases it. A worker grows or shrinks metadata outside cache locks.

use alloc::vec::Vec;
use core::alloc::Layout;
#[cfg(test)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};

use hyper::mm::FallibleArc;
#[cfg(not(test))]
use hyper::sync::InterruptSpinLock;
#[cfg(test)]
use hyper::sync::SpinLock;

mod budget;
mod index;
mod key;
mod load;
mod maintenance;
mod memory;
mod read;
pub(crate) mod read_ahead;
mod read_pages;
mod state;
mod storage;

#[cfg(not(test))]
pub(crate) mod worker;

use budget::{Budget, OwnedPage};
pub(crate) use key::{
    CacheKey, ContentRevision, FileIdentity, FilePageIndex, FilesystemGeneration, NodeIdentity,
};
pub(crate) use load::{LoadReservation, PendingPage, Refill};
#[cfg(not(test))]
pub(crate) use maintenance::AdmissionPause;
pub(crate) use maintenance::{CacheUsage, Maintenance, ReclaimCursor, ReclaimScan};
pub(crate) use read::read_observed;
pub(crate) use read::{FilePage, ReadError, read};
#[cfg(not(test))]
pub(crate) use read_pages::PageRead;
pub(crate) use read_pages::read_pages;

/// Initial live-payload allowance: loaders, resident pages, and evicted pins.
/// Each `FilePage` contains at most one page of bytes; payload headers and the
/// slot metadata are additional admitted service-owned allocations.
#[cfg(not(test))]
pub(crate) const SYSTEM_PAGE_CAPACITY: usize = 256;

#[cfg(not(test))]
type CacheLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;
#[cfg(test)]
type CacheLock<T> = SpinLock<T>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheError {
    Allocation,
    InvalidCapacity,
    SequenceExhausted,
    StaleLoad,
}

impl From<state::Error> for CacheError {
    fn from(error: state::Error) -> Self {
        match error {
            state::Error::Allocation => Self::Allocation,
            state::Error::InvalidCapacity => Self::InvalidCapacity,
            state::Error::SequenceExhausted => Self::SequenceExhausted,
            state::Error::StaleLoad => Self::StaleLoad,
        }
    }
}

/// Read-only shared ownership of one published payload.
pub(crate) struct CachedPage<Page> {
    inner: FallibleArc<OwnedPage<Page>>,
}

impl<Page> CachedPage<Page> {
    pub(crate) fn value(&self) -> &Page {
        &self.inner.value
    }
}

impl<Page> Clone for CachedPage<Page> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Immutable storage attached to a hardware file mapping. Its cache key must
/// remain discoverable until every mapping owner retires. Ordinary short read
/// pins may still outlive cache eviction without imposing this constraint.
pub(crate) struct MappedPage<Page> {
    inner: FallibleArc<OwnedPage<Page>>,
}
impl<Page> MappedPage<Page> {
    pub(crate) fn value(&self) -> &Page {
        &self.inner.value
    }
}
impl<Page> Drop for MappedPage<Page> {
    fn drop(&mut self) {
        self.inner.release_mapping();
    }
}

/// Result of a cache access before backend fill.
pub(crate) enum CacheAccess<'cache, Page> {
    Hit(CachedPage<Page>),
    /// Another caller owns the fill for this exact key.
    LoadInProgress,
    /// This caller owns the only admissible fill for the selected slot.
    Load(LoadReservation<'cache, Page>),
    /// All slots are loading, or live payloads exhaust the allocation budget.
    CapacityBusy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct CacheSnapshot {
    pub(crate) capacity: usize,
    pub(crate) clean: usize,
    pub(crate) loading: usize,
    pub(crate) live_payloads: usize,
    pub(crate) payload_creations: usize,
    pub(crate) payload_reuses: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
    pub(crate) in_progress: u64,
    pub(crate) capacity_busy: u64,
}

struct CacheState<Page> {
    model: state::State,
    pages: Vec<Option<FallibleArc<OwnedPage<Page>>>>,
    metadata_pages: usize,
}

impl<Page> CacheState<Page> {
    fn try_new(capacity: usize) -> Result<Self, CacheError> {
        Self::try_new_for_resize(capacity, false)
    }

    fn try_new_for_resize(capacity: usize, shrinking: bool) -> Result<Self, CacheError> {
        let [slots, heads, next, free, entries, positions] =
            state::State::allocation_layouts(capacity)?;
        let pages_layout = Layout::array::<Option<FallibleArc<OwnedPage<Page>>>>(capacity)
            .map_err(|_| CacheError::InvalidCapacity)?;
        let (reservation, metadata_pages) = memory::reserve_table(
            &[slots, heads, next, free, entries, positions, pages_layout],
            shrinking,
        )?;
        let model = state::State::try_new(capacity)?;
        let mut pages = memory::vector(capacity)?;
        pages.resize_with(capacity, || None);
        // Every backing allocation is now reflected in physical free memory.
        // Release pending admission only after construction or full rollback.
        drop(reservation);
        Ok(Self {
            model,
            pages,
            metadata_pages,
        })
    }
}

struct CacheControl<Page> {
    table: Option<CacheState<Page>>,
    maintenance_pending: bool,
    maintenance_running: bool,
    generation: u64,
    admission_pauses: usize,
}

/// Immutable I/O page cache with worker-managed metadata capacity. Typed keys
/// distinguish file data, directory batches, and name/attribute records; all
/// payload kinds share the same admission and pressure-reclaim policy.
pub(crate) struct FileDataCache<Page> {
    state: CacheLock<CacheControl<Page>>,
    budget: FallibleArc<Budget>,
    admission: AtomicBool,
    growth_requested: AtomicBool,
    #[cfg(test)]
    fail_publication: AtomicBool,
    #[cfg(test)]
    fail_maintenance: AtomicBool,
    #[cfg(test)]
    payload_creations: AtomicUsize,
    #[cfg(test)]
    payload_reuses: AtomicUsize,
}

impl<Page> FileDataCache<Page> {
    /// Speculation is optional even when a demand read could grow the cache.
    #[cfg(not(test))]
    pub(crate) fn prefetch_allowed(&self) -> bool {
        self.admission.load(Ordering::Acquire)
            && memory::admission_allowed()
            && self.state.with(|control| {
                control.admission_pauses == 0
                    && !control.maintenance_pending
                    && control.table.is_some()
            })
    }

    #[cfg(not(test))]
    pub(crate) fn try_new_system() -> Result<Self, CacheError> {
        match Self::try_new(SYSTEM_PAGE_CAPACITY) {
            Ok(cache) => Ok(cache),
            Err(CacheError::Allocation) => {
                // Caching is optional. Failure to provision its initial arrays
                // must not prevent VFS startup; demand can enable it later.
                // This small control owner is still conventionally fallible.
                let budget = memory::shared(Budget::new(0))?;
                Ok(Self::from_parts(None, budget))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn try_new(capacity: usize) -> Result<Self, CacheError> {
        let table = CacheState::try_new(capacity)?;
        let (reservation, _) = memory::reserve(&[budget_layout()])?;
        let budget = memory::shared(Budget::new(capacity))?;
        drop(reservation);
        Ok(Self::from_parts(Some(table), budget))
    }

    fn from_parts(table: Option<CacheState<Page>>, budget: FallibleArc<Budget>) -> Self {
        Self {
            state: CacheLock::new(CacheControl {
                table,
                maintenance_pending: false,
                maintenance_running: false,
                generation: 0,
                admission_pauses: 0,
            }),
            budget,
            admission: AtomicBool::new(true),
            growth_requested: AtomicBool::new(false),
            #[cfg(test)]
            fail_publication: AtomicBool::new(false),
            #[cfg(test)]
            fail_maintenance: AtomicBool::new(false),
            #[cfg(test)]
            payload_creations: AtomicUsize::new(0),
            #[cfg(test)]
            payload_reuses: AtomicUsize::new(0),
        }
    }

    /// A miss neither reserves a loader nor evicts another file's page.
    pub(crate) fn lookup(&self, key: CacheKey) -> Result<Option<CachedPage<Page>>, CacheError> {
        self.state.with(|control| {
            let Some(cache) = control.table.as_mut() else {
                return Ok(None);
            };
            let Some(slot) = cache.model.lookup(key)? else {
                return Ok(None);
            };
            let Some(page) = cache.pages.get(slot).and_then(Option::as_ref) else {
                cache_invariant_violation();
            };
            Ok(Some(CachedPage {
                inner: page.clone(),
            }))
        })
    }

    /// Pin key and bytes in one cache-lock interval, serialized with eviction.
    /// A detached table temporarily has no discoverable key; callers can retry
    /// or use independent private backing without inventing a second cache key.
    pub(crate) fn lookup_mapping(
        &self,
        key: CacheKey,
    ) -> Result<Option<MappedPage<Page>>, CacheError> {
        self.state.with(|control| {
            let Some(cache) = control.table.as_mut() else {
                return Ok(None);
            };
            let Some(slot) = cache.model.lookup(key)? else {
                return Ok(None);
            };
            let page = cache.pages[slot]
                .as_ref()
                .unwrap_or_else(|| cache_invariant_violation());
            page.retain_mapping();
            Ok(Some(MappedPage {
                inner: page.clone(),
            }))
        })
    }

    /// Looks up a page or reserves exactly one generation-tagged fill.
    ///
    /// The cache lock covers only the bounded state transition and shared-owner
    /// clone. Backend access, allocation, copying, and destruction occur after
    /// the lock is released.
    pub(crate) fn access(&self, key: CacheKey) -> Result<CacheAccess<'_, Page>, CacheError> {
        let admission = self.admission.load(Ordering::Acquire) && memory::admission_allowed();
        let outcome = self
            .state
            .with(|control| -> Result<LockedAccess<Page>, CacheError> {
                let allow_load =
                    admission && !control.maintenance_pending && control.admission_pauses == 0;
                let Some(cache) = control.table.as_mut() else {
                    if admission && !control.maintenance_running {
                        self.growth_requested.store(true, Ordering::Release);
                    }
                    return Ok(LockedAccess::CapacityBusy);
                };
                let access = cache.model.access_with_victim(key, allow_load, |slot| {
                    cache.pages[slot]
                        .as_ref()
                        .is_some_and(|page| !page.mapping_pinned())
                })?;
                match access {
                    state::Access::Hit { slot } => {
                        let Some(page) = cache.pages.get(slot).and_then(Option::as_ref) else {
                            cache_invariant_violation();
                        };
                        Ok(LockedAccess::Hit(CachedPage {
                            inner: page.clone(),
                        }))
                    }
                    state::Access::LoadInProgress => Ok(LockedAccess::LoadInProgress),
                    state::Access::Reserved { token, evicted } => {
                        if evicted {
                            self.growth_requested.store(true, Ordering::Release);
                        }
                        let Some(page) = cache.pages.get_mut(token.slot()) else {
                            cache_invariant_violation();
                        };
                        let old = page.take();
                        if old.is_some() != evicted {
                            cache_invariant_violation();
                        }
                        Ok(LockedAccess::Load { token, old })
                    }
                    state::Access::CapacityBusy => {
                        if allow_load {
                            self.growth_requested.store(true, Ordering::Release);
                        }
                        Ok(LockedAccess::CapacityBusy)
                    }
                }
            })?;
        if self.growth_requested.load(Ordering::Acquire) {
            notify_worker();
        }
        Ok(match outcome {
            LockedAccess::Hit(page) => CacheAccess::Hit(page),
            LockedAccess::LoadInProgress => CacheAccess::LoadInProgress,
            LockedAccess::Load { token, old } => {
                // No Weak owner of an OwnedPage is ever issued: CachedPage
                // exposes only its value. Unique conversion and destruction
                // happen outside the lock, and pinned readers keep their bytes.
                let recycled = old.and_then(|page| match page.try_into_unique() {
                    Ok(page) => Some(load::FillStorage::Reused(page)),
                    Err(page) => {
                        drop(page);
                        None
                    }
                });
                let storage = recycled
                    .or_else(|| Budget::reserve(&self.budget).map(load::FillStorage::Fresh));
                match storage {
                    Some(storage) => CacheAccess::Load(LoadReservation::new(self, token, storage)),
                    None => {
                        self.state.with(|control| {
                            let Some(cache) = control.table.as_mut() else {
                                cache_invariant_violation();
                            };
                            if cache.model.abort(token).is_err() {
                                cache_invariant_violation();
                            }
                            cache.model.record_capacity_busy();
                        });
                        self.load_finished();
                        CacheAccess::CapacityBusy
                    }
                }
            }
            LockedAccess::CapacityBusy => CacheAccess::CapacityBusy,
        })
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> CacheSnapshot {
        let snapshot = self
            .state
            .with(|control| control.table.as_ref().map(|cache| cache.model.snapshot()));
        let snapshot = snapshot.unwrap_or(state::Snapshot {
            capacity: 0,
            clean: 0,
            loading: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            in_progress: 0,
            capacity_busy: 0,
        });
        CacheSnapshot {
            capacity: snapshot.capacity,
            clean: snapshot.clean,
            loading: snapshot.loading,
            live_payloads: self.budget.used(),
            payload_creations: self.payload_creations.load(Ordering::Relaxed),
            payload_reuses: self.payload_reuses.load(Ordering::Relaxed),
            hits: snapshot.hits,
            misses: snapshot.misses,
            evictions: snapshot.evictions,
            in_progress: snapshot.in_progress,
            capacity_busy: snapshot.capacity_busy,
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_publication_for_test(&self) {
        self.fail_publication.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn exhaust_sequences_for_test(&self) {
        self.state.with(|control| {
            if let Some(cache) = control.table.as_mut() {
                cache.model.exhaust_sequences_for_test();
            }
        });
    }
}

fn budget_layout() -> Layout {
    Layout::from_size_align(
        FallibleArc::<Budget>::allocation_size(),
        core::mem::align_of::<Budget>(),
    )
    .unwrap_or_else(|_| cache_invariant_violation())
}

fn notify_worker() {
    #[cfg(not(test))]
    worker::request();
}

enum LockedAccess<Page> {
    Hit(CachedPage<Page>),
    LoadInProgress,
    Load {
        token: state::LoadToken,
        old: Option<FallibleArc<OwnedPage<Page>>>,
    },
    CapacityBusy,
}

#[cold]
fn cache_invariant_violation() -> ! {
    hyper::debug::invariant_failure("I/O cache ownership invariant")
}
