// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free census of cached tokens versus central slab reservations.

#[derive(Clone, Copy)]
struct Entry {
    page: u64,
    reserved: usize,
}

pub(super) struct CacheSnapshot<const OBJECTS: usize, const CPUS: usize> {
    entries: [Entry; OBJECTS],
    count: usize,
    pub(super) epochs: [u64; CPUS],
}

impl<const OBJECTS: usize, const CPUS: usize> CacheSnapshot<OBJECTS, CPUS> {
    pub(super) const fn new() -> Self {
        Self {
            entries: [Entry {
                page: 0,
                reserved: 0,
            }; OBJECTS],
            count: 0,
            epochs: [0; CPUS],
        }
    }

    pub(super) fn clear(&mut self) {
        self.count = 0;
    }

    pub(super) fn record(&mut self, page: u64, reserved: usize) -> Option<()> {
        let entry = self.entries.get_mut(self.count)?;
        *entry = Entry { page, reserved };
        self.count += 1;
        Some(())
    }

    /// Call only after establishing an unchanged cache-epoch interval around
    /// the capture. A missing token can be a caller or an in-flight transfer;
    /// neither permits reclaiming its page. Free central slots need no tokens.
    pub(super) fn reclaimable_pages(&mut self) -> Option<usize> {
        let entries = &mut self.entries[..self.count];
        entries.sort_unstable_by_key(|entry| entry.page);
        let mut reclaimed = 0;
        let mut index = 0;
        while index < entries.len() {
            let first = entries[index];
            let end = entries[index..].partition_point(|entry| entry.page == first.page) + index;
            if first.reserved == 0
                || entries[index..end]
                    .iter()
                    .any(|entry| entry.reserved != first.reserved)
            {
                return None;
            }
            let cached = end - index;
            if cached > first.reserved {
                return None;
            }
            if cached == first.reserved {
                reclaimed += 1;
            }
            index = end;
        }
        Some(reclaimed)
    }
}
