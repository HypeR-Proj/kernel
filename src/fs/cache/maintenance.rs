// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Worker-driven table resizing and bounded clean-page reclamation.

use core::sync::atomic::Ordering;

use super::{CacheError, CacheState, FileDataCache, cache_invariant_violation, notify_worker};

const RECLAIM_BATCH: usize = 64;

#[cfg(test)]
#[path = "../../../tests/host/src/cases/file_cache_maintenance.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CacheUsage {
    pub(crate) table_present: bool,
    pub(crate) admission_enabled: bool,
    pub(crate) maintenance_running: bool,
    pub(crate) capacity: usize,
    pub(crate) clean: usize,
    pub(crate) loading: usize,
    pub(crate) live_payloads: usize,
    pub(crate) growth_requested: bool,
    pub(crate) maintenance_pending: bool,
    pub(crate) metadata_pages: usize,
    pub(crate) admission_paused: bool,
}

/// Independent scheduled-reclaim admission barrier. Pressure updates and table
/// maintenance cannot reopen it before its requesting allocation is retried.
#[must_use]
pub(crate) struct AdmissionPause<'cache, Page> {
    cache: &'cache FileDataCache<Page>,
}

impl<Page> Drop for AdmissionPause<'_, Page> {
    fn drop(&mut self) {
        self.cache.state.with(|control| {
            control.admission_pauses = control
                .admission_pauses
                .checked_sub(1)
                .unwrap_or_else(|| cache_invariant_violation());
        });
        notify_worker();
    }
}

/// One finite sweep of the table incarnation observed on its first batch.
pub(crate) struct ReclaimCursor {
    generation: Option<u64>,
    capacity: usize,
    next: usize,
    finished: bool,
}

impl ReclaimCursor {
    pub(crate) const fn new() -> Self {
        Self {
            generation: None,
            capacity: 0,
            next: 0,
            finished: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReclaimScan {
    pub(crate) inspected: usize,
    pub(crate) detached: usize,
    pub(crate) finished: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Maintenance {
    Unchanged,
    Busy,
    Resized,
}

impl<Page> FileDataCache<Page> {
    pub(crate) fn usage(&self) -> super::CacheUsage {
        self.state.with(|control| {
            let (capacity, clean, loading, metadata_pages) =
                control.table.as_ref().map_or((0, 0, 0, 0), |cache| {
                    (
                        cache.model.capacity(),
                        cache.model.clean(),
                        cache.model.loading(),
                        cache.metadata_pages,
                    )
                });
            CacheUsage {
                table_present: control.table.is_some(),
                admission_enabled: self.admission.load(Ordering::Acquire),
                maintenance_running: control.maintenance_running,
                capacity,
                clean,
                loading,
                live_payloads: self.budget.used(),
                growth_requested: self.growth_requested.load(Ordering::Acquire),
                maintenance_pending: control.maintenance_pending,
                metadata_pages,
                admission_paused: control.admission_pauses != 0,
            }
        })
    }

    /// Pressure policy is separate from the temporary maintenance freeze.
    pub(crate) fn set_admission(&self, allowed: bool) {
        self.admission.store(allowed, Ordering::Release);
    }

    pub(crate) fn pause_admission(&self) -> AdmissionPause<'_, Page> {
        // The same lock guards reservation, so no new loader can slip between
        // publishing this pause and its caller starting a scheduled scan.
        self.state.with(|control| {
            control.admission_pauses = control
                .admission_pauses
                .checked_add(1)
                .unwrap_or_else(|| cache_invariant_violation());
        });
        AdmissionPause { cache: self }
    }

    pub(crate) fn acknowledge_growth(&self) {
        self.growth_requested.store(false, Ordering::Release);
    }

    pub(crate) fn cancel_maintenance(&self) {
        self.state.with(|control| {
            if !control.maintenance_running {
                control.maintenance_pending = false;
            }
        });
    }

    /// Freeze new loaders, then move the table out in constant lock time.
    /// Readers use direct I/O while maintenance owns the table. Existing page
    /// pins remain independently charged and immutable throughout migration.
    pub(crate) fn maintain_capacity(&self, capacity: usize) -> Result<Maintenance, CacheError> {
        super::state::State::allocation_layouts(capacity)?;
        let parked = self.state.with(|control| {
            if control.maintenance_running || control.admission_pauses != 0 {
                return Ok(None);
            }
            if control
                .table
                .as_ref()
                .is_some_and(|cache| cache.model.capacity() == capacity)
            {
                control.maintenance_pending = false;
                return Ok(Some(None));
            }
            control.maintenance_pending = true;
            if control
                .table
                .as_ref()
                .is_some_and(|cache| cache.model.loading() != 0)
            {
                return Ok(None);
            }
            let Some(generation) = control.generation.checked_add(1) else {
                control.maintenance_pending = false;
                return Err(CacheError::SequenceExhausted);
            };
            control.generation = generation;
            control.maintenance_running = true;
            Ok(Some(Some(control.table.take())))
        })?;
        let Some(parked) = parked else {
            return Ok(Maintenance::Busy);
        };
        let Some(table) = parked else {
            self.acknowledge_growth();
            return Ok(Maintenance::Unchanged);
        };
        let mut maintenance = ParkedTable { cache: self, table };
        // Parking excludes new mapping pins. Existing pins may disappear, but
        // every remaining canonical key must survive the replacement table.
        let pinned = maintenance.table.as_ref().map_or(0, |cache| {
            cache
                .pages
                .iter()
                .flatten()
                .filter(|page| page.mapping_pinned())
                .count()
        });
        let mut capacity = capacity.max(
            pinned
                .max(1)
                .checked_next_power_of_two()
                .ok_or(CacheError::InvalidCapacity)?,
        );
        let old_capacity = maintenance
            .table
            .as_ref()
            .map_or(0, |cache| cache.model.capacity());
        if capacity == old_capacity {
            // Mapped pages already require the complete old index. Do not
            // allocate an identical replacement during a pressure episode.
            self.acknowledge_growth();
            return Ok(Maintenance::Unchanged);
        }
        let shrinking = capacity < old_capacity;

        // Empty metadata must remain reclaimable even when the pressure floor
        // rejects replacement allocation. If rebuilding fails, Drop publishes
        // an explicitly disabled cache; the next safe demand can enable it.
        if maintenance
            .table
            .as_ref()
            .is_some_and(|cache| cache.model.clean() == 0 && capacity < cache.model.capacity())
        {
            drop(maintenance.table.take());
            self.budget.set_capacity(0);
        }
        #[cfg(test)]
        if self.fail_maintenance.swap(false, Ordering::AcqRel) {
            return Err(CacheError::Allocation);
        }
        let mut replacement = loop {
            let mut candidate = CacheState::try_new_for_resize(capacity, shrinking)?;
            let mut fits = true;
            if let Some(old) = maintenance.table.as_ref() {
                for slot in 0..old.model.capacity() {
                    if old.pages[slot]
                        .as_ref()
                        .is_some_and(|page| page.mapping_pinned())
                    {
                        let Some((key, sequence)) = old.model.clean_entry(slot) else {
                            cache_invariant_violation();
                        };
                        if candidate.model.restore_clean(key, sequence).is_none() {
                            fits = false;
                            break;
                        }
                    }
                }
            }
            if fits {
                break candidate;
            }
            // Bounded hash chains may need more buckets than the pin count.
            // Never evict a mapped key or destroy the old index on failure.
            if !shrinking || capacity >= old_capacity / 2 {
                return Ok(Maintenance::Busy);
            }
            capacity *= 2;
        };
        if let Some(old) = maintenance.table.as_mut() {
            // Move canonical mapped keys first, leaving only evictable entries
            // in the old LRU. No fallible operation follows this commit point.
            for slot in 0..old.model.capacity() {
                if let Some((key, _)) = old.model.clean_entry(slot)
                    && let Some(new_slot) = replacement.model.peek_clean(key)
                {
                    replacement.pages[new_slot] = old.pages[slot].take();
                    old.model.detach_clean(slot);
                }
            }
            while old.model.clean() > capacity - replacement.model.clean() {
                let Some(slot) = old.model.oldest_clean() else {
                    cache_invariant_violation();
                };
                old.model.remove_clean(slot);
                drop(old.pages[slot].take());
            }
            for slot in 0..old.model.capacity() {
                if let Some((key, sequence)) = old.model.clean_entry(slot)
                    && replacement.model.peek_clean(key).is_none()
                {
                    let _ = replacement.model.restore_clean(key, sequence);
                }
            }
            for slot in 0..old.model.capacity() {
                if let Some((key, _)) = old.model.clean_entry(slot) {
                    if let Some(new_slot) = replacement.model.peek_clean(key) {
                        replacement.pages[new_slot] = old.pages[slot].take();
                    } else {
                        old.model.remove_clean(slot);
                    }
                }
            }
            replacement.model.inherit_history(&old.model);
        }
        // Replaced arrays and evicted owners are destroyed here, outside the
        // cache lock. The guard restores ownership on every return path.
        maintenance.table = Some(replacement);
        self.acknowledge_growth();
        drop(maintenance);
        Ok(Maintenance::Resized)
    }

    pub(super) fn load_finished(&self) {
        let notify = self.state.with(|control| {
            control.maintenance_pending
                && !control.maintenance_running
                && control
                    .table
                    .as_ref()
                    .is_some_and(|cache| cache.model.loading() == 0)
        });
        if notify {
            notify_worker();
        }
        #[cfg(not(test))]
        super::worker::load_finished();
    }

    /// Detaches at most 64 cold entries. A reader pin can postpone physical
    /// reclamation; the return value counts detached entries, not freed pages.
    pub(crate) fn reclaim_batch(&self, limit: usize) -> usize {
        let mut detached = 0;
        for _ in 0..limit.min(RECLAIM_BATCH) {
            let page = self.state.with(|control| {
                let cache = control.table.as_mut()?;
                let slot = (0..cache.model.clean().min(RECLAIM_BATCH))
                    .filter_map(|index| cache.model.cold_candidate(index))
                    .find(|slot| {
                        cache.pages[*slot]
                            .as_ref()
                            .is_some_and(|page| !page.mapping_pinned())
                    })?;
                cache.model.remove_clean(slot);
                cache.pages[slot].take()
            });
            let Some(page) = page else {
                break;
            };
            drop(page);
            detached += 1;
        }
        detached
    }

    /// Allocation-context reclamation never waits for the cache lock and only
    /// destroys unpinned payloads. The caller must prove `Page`'s destructor is
    /// bounded, allocation-free, and takes no subsystem locks. Production uses
    /// only `FilePage` with the separately audited VFS `FileRecord` sidecar.
    /// Returns destroyed payloads (one physical page each in production).
    pub(crate) fn try_reclaim_batch(&self, limit: usize) -> usize {
        let mut reclaimed = 0;
        let mut candidate = 0;
        for _ in 0..limit.min(RECLAIM_BATCH) {
            let selected = self.state.try_with(|control| {
                let cache = control.table.as_mut()?;
                let slot = cache.model.cold_candidate(candidate)?;
                let Some(page) = cache.pages[slot].as_ref() else {
                    cache_invariant_violation();
                };
                if page.strong_count() != 1 {
                    return Some(None);
                }
                cache.model.remove_clean(slot);
                Some(cache.pages[slot].take())
            });
            let Some(Some(selected)) = selected else {
                break;
            };
            let Some(page) = selected else {
                candidate += 1;
                continue;
            };
            // The cache held the only strong reference and never issues Weak
            // owners. Removal prevents subsequent readers from cloning it.
            let unique = match page.try_into_unique() {
                Ok(unique) => unique,
                Err(_) => cache_invariant_violation(),
            };
            drop(unique);
            reclaimed += 1;
            candidate = 0;
        }
        reclaimed
    }

    /// Scheduled demand reclaim visits each numeric slot at most once, even
    /// when it is vacant, loading, pinned, or outside the caller's scope. A
    /// changed or parked table ends the sweep; this never waits for loaders or
    /// pins. Matchers and payload destruction run outside the cache lock.
    pub(crate) fn reclaim_scan(
        &self,
        cursor: &mut ReclaimCursor,
        limit: usize,
        mut matches: impl FnMut(&Page) -> bool,
    ) -> super::ReclaimScan {
        if cursor.generation.is_none() && !cursor.finished {
            let initial = self.state.with(|control| {
                Some((control.generation, control.table.as_ref()?.model.capacity()))
            });
            if let Some((generation, capacity)) = initial {
                cursor.generation = Some(generation);
                cursor.capacity = capacity;
            } else {
                cursor.finished = true;
            }
        }
        let mut inspected = 0;
        let mut detached = 0;
        while !cursor.finished && inspected < limit.min(RECLAIM_BATCH) {
            if cursor.next == cursor.capacity {
                cursor.finished = true;
                break;
            }
            let slot = cursor.next;
            cursor.next += 1;
            inspected += 1;
            let candidate = self.state.with(|control| {
                if Some(control.generation) != cursor.generation {
                    return Err(());
                }
                let cache = control.table.as_ref().ok_or(())?;
                Ok(cache.pages[slot].clone())
            });
            let candidate = match candidate {
                Ok(Some(candidate)) => candidate,
                Ok(None) => continue,
                Err(()) => {
                    cursor.finished = true;
                    break;
                }
            };
            if matches(&candidate.value) {
                let retired = self.state.with(|control| {
                    if Some(control.generation) != cursor.generation {
                        return None;
                    }
                    let cache = control.table.as_mut()?;
                    let same = cache.pages[slot]
                        .as_ref()
                        .is_some_and(|page| core::ptr::eq(&**page, &*candidate));
                    // Hardware file mappings retain their canonical cache key;
                    // ordinary read pins may safely outlive eviction.
                    if !same || candidate.mapping_pinned() {
                        return None;
                    }
                    cache.model.remove_clean(slot);
                    cache.pages[slot].take()
                });
                detached += usize::from(retired.is_some());
                drop(retired);
            }
            drop(candidate);
        }
        cursor.finished |= cursor.next == cursor.capacity;
        ReclaimScan {
            inspected,
            detached,
            finished: cursor.finished,
        }
    }

    /// An explicit diagnostic sample, never used for admission or worker
    /// decisions. Each lock interval inspects at most 64 entries; concurrent
    /// reader changes are observations, not one global ownership snapshot.
    pub(crate) fn reclaimable_pages(&self) -> Option<usize> {
        let (generation, capacity) = self
            .state
            .with(|control| Some((control.generation, control.table.as_ref()?.model.capacity())))?;
        let mut total = 0;
        for start in (0..capacity).step_by(RECLAIM_BATCH) {
            total += self.state.with(|control| {
                if control.generation != generation {
                    return None;
                }
                let cache = control.table.as_ref()?;
                Some(
                    cache.pages[start..(start + RECLAIM_BATCH).min(capacity)]
                        .iter()
                        .filter(|page| page.as_ref().is_some_and(|page| page.strong_count() == 1))
                        .count(),
                )
            })?;
        }
        Some(total)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_maintenance_for_test(&self) {
        self.fail_maintenance.store(true, Ordering::Release);
    }
}

/// Owns the parked state until restoration, including allocation-error exits.
struct ParkedTable<'cache, Page> {
    cache: &'cache FileDataCache<Page>,
    table: Option<CacheState<Page>>,
}

impl<Page> Drop for ParkedTable<'_, Page> {
    fn drop(&mut self) {
        let capacity = self
            .table
            .as_ref()
            .map_or(0, |cache| cache.model.capacity());
        self.cache.budget.set_capacity(capacity);
        self.cache.state.with(|control| {
            if control.table.is_some() || !control.maintenance_running {
                cache_invariant_violation();
            }
            control.table = self.table.take();
            control.maintenance_running = false;
            control.maintenance_pending = false;
        });
    }
}
