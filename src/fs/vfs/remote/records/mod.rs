// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Individually charged bindings indexed by ID and canonical path. Index nodes
//! retire with their binding, with no array capacity left at a high-water mark.

use alloc::{boxed::Box, string::String};

use super::{Error, Record};

mod tree;
use tree::{Node, Tree};

pub(super) struct PreparedRecord {
    id: Box<Node<u64, Record>>,
    path: Box<Node<(u64, u64), ()>>,
}

/// The volume mutex covers both indices and every record change. Deletion and
/// path replacement reuse owned index nodes; neither allocates during reclaim
/// or after a backend namespace mutation has committed.
pub(super) struct Records {
    ids: Tree<u64, Record>,
    paths: Tree<(u64, u64), ()>,
    cursor: Option<u64>,
    len: usize,
}

impl Records {
    pub(super) const fn new() -> Self {
        Self {
            ids: Tree::new(),
            paths: Tree::new(),
            cursor: None,
            len: 0,
        }
    }

    pub(super) const fn allocation_size() -> usize {
        core::mem::size_of::<Node<u64, Record>>() + core::mem::size_of::<Node<(u64, u64), ()>>()
    }

    pub(super) fn prepare(record: Record) -> Result<PreparedRecord, Error> {
        let key = (path_hash(&record.path), record.id);
        let id = hyper::mm::try_box(Node::new(record.id, record)).map_err(Error::from)?;
        let path = hyper::mm::try_box(Node::new(key, ())).map_err(Error::from)?;
        Ok(PreparedRecord { id, path })
    }

    pub(super) fn insert(&mut self, record: PreparedRecord) {
        self.ids.insert(record.id);
        self.paths.insert(record.path);
        self.len += 1;
    }

    pub(super) const fn len(&self) -> usize {
        self.len
    }

    pub(super) fn cleanup_bound(&self) -> usize {
        self.len
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Record> {
        let mut previous = None;
        core::iter::from_fn(move || {
            let (id, record) = self.ids.next(previous)?;
            previous = Some(id);
            Some(record)
        })
    }

    pub(super) fn get(&self, id: u64) -> Option<&Record> {
        self.ids.get(id)
    }

    /// Paths participate in a second index. Change a path only via `set_path`.
    pub(super) fn get_mut(&mut self, id: u64) -> Option<&mut Record> {
        self.ids.get_mut(id)
    }

    pub(super) fn find_path(&self, path: &str) -> Option<&Record> {
        let hash = path_hash(path);
        // ID zero is legal, so start in the preceding hash bucket instead of
        // skipping the possible root key (hash, 0).
        let mut previous = hash.checked_sub(1).map(|hash| (hash, u64::MAX));
        while let Some((key, ())) = self.paths.next(previous) {
            if key.0 != hash {
                break;
            }
            let record = self.ids.get(key.1)?;
            // Hashes locate candidates; the full canonical path determines
            // identity even when several names have the same placement hash.
            if record.path == path {
                return Some(record);
            }
            previous = Some(key);
        }
        None
    }

    pub(super) fn path(&self, id: u64) -> Result<&str, Error> {
        self.get(id)
            .map(|record| record.path.as_str())
            .ok_or(Error::Missing)
    }

    pub(super) fn set_path(&mut self, id: u64, path: String) -> Result<(), Error> {
        let record = self.ids.get_mut(id).ok_or(Error::Missing)?;
        let Some(mut index) = self.paths.remove((path_hash(&record.path), id)) else {
            invariant_failure();
        };
        index.key = (path_hash(&path), id);
        record.path = path;
        self.paths.insert(index);
        Ok(())
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<PreparedRecord> {
        let record = self.ids.remove(id)?;
        let Some(path) = self.paths.remove((path_hash(&record.value.path), id)) else {
            invariant_failure();
        };
        self.len -= 1;
        Some(PreparedRecord { id: record, path })
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Record) -> bool) {
        let mut previous = None;
        while let Some((id, record)) = self.ids.next(previous) {
            let retained = keep(record);
            previous = Some(id);
            if !retained {
                drop(self.remove(id));
            }
        }
    }

    /// Inspect one successor per call. The numeric cursor survives deletion
    /// and rotations without a pointer pin, allocation, or rescanning a prefix.
    pub(super) fn inspect_next(&mut self) -> Option<Option<PreparedRecord>> {
        let (id, record) = self.ids.next(self.cursor).or_else(|| self.ids.next(None))?;
        let alive = record.content.is_alive();
        self.cursor = Some(id);
        Some(if alive { None } else { self.remove(id) })
    }
}

impl PreparedRecord {
    pub(super) fn value(&self) -> &Record {
        &self.id.value
    }

    pub(super) fn set_path(&mut self, path: String) {
        self.path.key = (path_hash(&path), self.id.key);
        self.id.value.path = path;
    }
}

fn path_hash(path: &str) -> u64 {
    path.as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

#[cold]
fn invariant_failure() -> ! {
    hyper::debug::invariant_failure("filesystem binding indices disagree")
}
