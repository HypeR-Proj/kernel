// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Indexed, eagerly maintained virtual-interrupt ready queue.

use core::cmp::Ordering;
pub(super) use hyper::collections::bounded_vec::BoundedVec;
use {hyper::collections::indexed_heap, hyper::collections::indexed_heap::Storage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct EntryIndex(pub(super) u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReadyRank {
    pub(super) active: bool,
    pub(super) priority: u8,
    pub(super) interrupt: u32,
}

impl Ord for ReadyRank {
    fn cmp(&self, other: &Self) -> Ordering {
        (!self.active, self.priority, self.interrupt).cmp(&(
            !other.active,
            other.priority,
            other.interrupt,
        ))
    }
}

impl PartialOrd for ReadyRank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(super) trait ReadyEntries {
    fn rank(&self, index: EntryIndex) -> ReadyRank;
    fn position(&self, index: EntryIndex) -> Option<usize>;
    fn set_position(&mut self, index: EntryIndex, position: Option<usize>);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadyError {
    Allocation,
    Capacity,
    CorruptPosition,
}

impl From<hyper::collections::bounded_vec::Error> for ReadyError {
    fn from(error: hyper::collections::bounded_vec::Error) -> Self {
        match error {
            hyper::collections::bounded_vec::Error::Allocation => Self::Allocation,
            hyper::collections::bounded_vec::Error::Capacity => Self::Capacity,
        }
    }
}

pub(super) struct ReadyQueue {
    entries: BoundedVec<EntryIndex>,
}

impl ReadyQueue {
    pub(super) fn try_with_capacity(capacity: usize) -> Result<Self, ReadyError> {
        Ok(Self {
            entries: BoundedVec::try_new(capacity)?,
        })
    }

    pub(super) fn allocation_size(&self) -> Option<usize> {
        self.entries.allocation_size()
    }

    pub(super) fn contains<S: ReadyEntries>(
        &self,
        index: EntryIndex,
        store: &S,
    ) -> Result<bool, ReadyError> {
        let Some(position) = store.position(index) else {
            return Ok(false);
        };
        if self.entries.get(position).copied() != Some(index) {
            return Err(ReadyError::CorruptPosition);
        }
        Ok(true)
    }

    pub(super) fn can_insert(&self) -> Result<(), ReadyError> {
        if self.entries.remaining() != 0 {
            Ok(())
        } else {
            Err(ReadyError::Capacity)
        }
    }

    pub(super) fn remaining(&self) -> usize {
        self.entries.remaining()
    }

    pub(super) fn first(&self) -> Option<EntryIndex> {
        self.entries.first().copied()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    pub(super) fn insert<S: ReadyEntries>(
        &mut self,
        index: EntryIndex,
        store: &mut S,
    ) -> Result<(), ReadyError> {
        self.can_insert()?;
        let position = self.entries.len();
        self.entries.push(index)?;
        store.set_position(index, Some(position));
        indexed_heap::sift_up(&mut HeapView { queue: self, store }, position);
        Ok(())
    }

    pub(super) fn remove<S: ReadyEntries>(&mut self, index: EntryIndex, store: &mut S) {
        let position = match store.position(index) {
            Some(position) => position,
            None => return,
        };
        let last = self.entries.len() - 1;
        self.swap(position, last, store);
        let _ = self.entries.pop();
        store.set_position(index, None);
        if position < self.entries.len() {
            self.repair(position, store);
        }
    }

    pub(super) fn reprioritize<S: ReadyEntries>(&mut self, index: EntryIndex, store: &mut S) {
        if let Some(position) = store.position(index) {
            self.repair(position, store);
        }
    }

    pub(super) fn pop<S: ReadyEntries>(&mut self, store: &mut S) -> Option<EntryIndex> {
        let index = self.entries.first().copied()?;
        self.remove(index, store);
        Some(index)
    }

    fn repair<S: ReadyEntries>(&mut self, position: usize, store: &mut S) {
        indexed_heap::repair(&mut HeapView { queue: self, store }, position);
    }

    fn swap<S: ReadyEntries>(&mut self, left: usize, right: usize, store: &mut S) {
        if left == right {
            return;
        }
        self.entries.swap(left, right);
        store.set_position(self.entries[left], Some(left));
        store.set_position(self.entries[right], Some(right));
    }
}

struct HeapView<'a, S> {
    queue: &'a mut ReadyQueue,
    store: &'a mut S,
}
impl<S: ReadyEntries> Storage for HeapView<'_, S> {
    fn len(&self) -> usize {
        self.queue.entries.len()
    }
    fn precedes(&self, left: usize, right: usize) -> bool {
        self.store.rank(self.queue.entries[left]) < self.store.rank(self.queue.entries[right])
    }
    fn swap(&mut self, left: usize, right: usize) {
        self.queue.swap(left, right, self.store);
    }
}
