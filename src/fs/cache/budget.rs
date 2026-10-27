// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Payload admission independent of cache residency and slot reuse.

use core::sync::atomic::{AtomicUsize, Ordering};

use hyper::mm::FallibleArc;

pub(super) struct Budget {
    capacity: AtomicUsize,
    used: AtomicUsize,
}

impl Budget {
    pub(super) const fn new(capacity: usize) -> Self {
        Self {
            capacity: AtomicUsize::new(capacity),
            used: AtomicUsize::new(0),
        }
    }

    pub(super) fn reserve(owner: &FallibleArc<Self>) -> Option<Permit> {
        owner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < owner.capacity.load(Ordering::Acquire)).then(|| used + 1)
            })
            .ok()?;
        Some(Permit {
            owner: owner.clone(),
        })
    }

    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub(super) fn set_capacity(&self, capacity: usize) {
        // A shrink may leave pinned readers above the new target. Their
        // permits remain charged until destruction; new admission stays shut.
        self.capacity.store(capacity, Ordering::Release);
    }
}

/// Retained from before allocation until the final payload owner is dropped.
pub(super) struct Permit {
    owner: FallibleArc<Budget>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.owner.used.fetch_sub(1, Ordering::AcqRel) == 0 {
            super::cache_invariant_violation();
        }
        #[cfg(not(test))]
        super::worker::page_released();
    }
}

pub(super) struct OwnedPage<Page> {
    // Field order releases the allocation before returning its budget permit.
    pub(super) value: Page,
    _permit: Permit,
    mappings: AtomicUsize,
}

impl<Page> OwnedPage<Page> {
    pub(super) const fn new(value: Page, permit: Permit) -> Self {
        Self {
            value,
            _permit: permit,
            mappings: AtomicUsize::new(0),
        }
    }

    pub(super) fn mapping_pinned(&self) -> bool {
        self.mappings.load(Ordering::Acquire) != 0
    }
    pub(super) fn retain_mapping(&self) {
        if self.mappings.fetch_add(1, Ordering::AcqRel) == usize::MAX {
            super::cache_invariant_violation();
        }
    }
    pub(super) fn release_mapping(&self) {
        let previous = self.mappings.fetch_sub(1, Ordering::AcqRel);
        if previous == 0 {
            super::cache_invariant_violation();
        }
        // The cache still owns this page, so releasing its last mapping need
        // not drop a payload or notify the allocator. Resume a pressure sweep
        // which previously stopped at this canonical key.
        #[cfg(not(test))]
        if previous == 1 {
            super::worker::page_released();
        }
    }

    pub(super) fn into_parts(self) -> (Page, Permit) {
        (self.value, self._permit)
    }
}
