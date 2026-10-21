// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::require_ok;
use hyper::mm::{
    PhysicalAddress,
    table_edit::{Editor, Error, Storage},
};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
struct Page {
    epoch: usize,
    words: Arc<[u64; 512]>,
}
#[derive(Clone)]
struct Memory<const LEVELS: usize> {
    pages: BTreeMap<u64, Page>,
    next: u64,
    epoch: usize,
    copies: usize,
    fail_after: Option<usize>,
}
impl<const L: usize> Memory<L> {
    fn new() -> (Self, PhysicalAddress) {
        let mut memory = Self {
            pages: BTreeMap::new(),
            next: 4096,
            epoch: 1,
            copies: 0,
            fail_after: None,
        };
        let root = require_ok(memory.writable(None));
        (memory, root)
    }
    fn fork(&self) -> Self {
        let mut result = self.clone();
        result.epoch += 1;
        result.copies = 0;
        result
    }
    fn lookup(&self, root: PhysicalAddress, address: u64) -> u64 {
        let mut table = root;
        for level in 0..L {
            let shift = 12 + 9 * (L - level - 1);
            let entry = require_ok(self.read(table, ((address >> shift) & 511) as usize));
            if entry == 0 || level + 1 == L {
                return entry;
            }
            table = crate::require_some(require_ok(Self::child(entry)));
        }
        unreachable!()
    }
}
impl<const L: usize> Storage for Memory<L> {
    const LEVELS: usize = L;
    fn read(&self, table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
        self.pages
            .get(&table.get())
            .and_then(|p| p.words.get(slot))
            .copied()
            .ok_or(Error::Storage)
    }
    fn write(&mut self, table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
        let page = self.pages.get_mut(&table.get()).ok_or(Error::Storage)?;
        assert_eq!(page.epoch, self.epoch, "published table was modified");
        let words = Arc::get_mut(&mut page.words).ok_or(Error::Storage)?;
        *words.get_mut(slot).ok_or(Error::Storage)? = value;
        Ok(())
    }
    fn writable(&mut self, old: Option<PhysicalAddress>) -> Result<PhysicalAddress, Error> {
        let words = match old {
            Some(old) => {
                let page = self.pages.get(&old.get()).ok_or(Error::Storage)?;
                if page.epoch == self.epoch {
                    return Ok(old);
                }
                *page.words
            }
            None => [0; 512],
        };
        if self.fail_after == Some(self.copies) {
            return Err(Error::Storage);
        }
        self.copies += 1;
        let physical = self.next;
        self.next += 4096;
        self.pages.insert(
            physical,
            Page {
                epoch: self.epoch,
                words: Arc::new(words),
            },
        );
        if let Some(old) = old {
            self.pages.remove(&old.get());
        }
        Ok(PhysicalAddress::new(physical))
    }
    fn discard(&mut self, table: PhysicalAddress) -> Result<(), Error> {
        self.pages
            .remove(&table.get())
            .map(|_| ())
            .ok_or(Error::Storage)
    }
    fn child(entry: u64) -> Result<Option<PhysicalAddress>, Error> {
        if entry == 0 {
            return Ok(None);
        }
        if entry & 4095 != 1 {
            return Err(Error::InvalidEntry);
        }
        Ok(Some(PhysicalAddress::new(entry & !4095)))
    }
    fn link(table: PhysicalAddress) -> u64 {
        table.get() | 1
    }
}

fn edits<const L: usize>() {
    let (mut original, mut root) = Memory::<L>::new();
    {
        let mut edit = require_ok(Editor::new(root, &mut original));
        for page in 0..16384u64 {
            require_ok(edit.set(page * 4096, ((page + 1) * 4096) | 3));
        }
        root = edit.root();
    }
    let original_tables = original.pages.len();
    let mut next = original.fork();
    let new_root = {
        let mut edit = require_ok(Editor::new(root, &mut next));
        require_ok(edit.set(0, 0xdead003));
        require_ok(edit.set(4096, 0xbeef003));
        edit.root()
    };
    assert_eq!(
        next.copies, L,
        "two nearby edits copy only one path, regardless of resident size"
    );
    assert_eq!(next.pages.len(), original_tables);
    assert_eq!(original.lookup(root, 0), 0x1003);
    assert_eq!(next.lookup(new_root, 0), 0xdead003);
    assert_eq!(next.lookup(new_root, 4096), 0xbeef003);
    assert_eq!(next.lookup(new_root, 8192), 0x3003);
    // Each intermediate allocation failure leaves the old hierarchy usable.
    for failure in 0..L {
        let mut failed = original.fork();
        failed.fail_after = Some(failure);
        assert_eq!(
            require_ok(Editor::new(root, &mut failed)).set(0, 0xdead003),
            Err(Error::Storage)
        );
        assert_eq!(original.lookup(root, 0), 0x1003);
    }
    {
        let mut edit = require_ok(Editor::new(new_root, &mut next));
        require_ok(edit.clear(4096, 4096));
        root = edit.root();
    }
    assert_eq!(next.lookup(root, 0), 0xdead003);
    assert_eq!(next.lookup(root, 4096), 0);
    // Full subtree removal drops every now-unreachable table, not its leaves
    // one at a time, and does not retain a chain of prior roots.
    {
        let mut edit = require_ok(Editor::new(root, &mut next));
        require_ok(edit.clear(0, 16384 * 4096));
        root = edit.root();
    }
    assert_eq!(next.pages.len(), 1);
    assert_eq!(next.lookup(root, 0), 0);
    assert_eq!(original.pages.len(), original_tables);
}

#[test]
fn edits_share_untouched_tables_and_retire_empty_paths() {
    edits::<3>();
    edits::<4>();
}

#[test]
fn sparse_edits_match_a_flat_map_across_retained_versions() {
    let (mut memory, mut root) = Memory::<4>::new();
    let mut expected = BTreeMap::new();
    let mut seed = 42u64;
    for step in 0..400 {
        let previous = memory.clone();
        let old_root = root;
        let before = expected.clone();
        memory = memory.fork();
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let page = (seed >> 32) % 4096;
        let mut edit = require_ok(Editor::new(root, &mut memory));
        if step % 3 == 0 {
            let end = (page + 63).min(4096);
            require_ok(edit.clear(page * 4096, (end - page) * 4096));
            expected.retain(|p, _| *p < page || *p >= end);
        } else {
            let entry = ((step + 1) * 4096) | 3;
            require_ok(edit.set(page * 4096, entry));
            expected.insert(page, entry);
        }
        root = edit.root();
        for page in 0..4096 {
            assert_eq!(
                memory.lookup(root, page * 4096),
                expected.get(&page).copied().unwrap_or(0)
            );
            assert_eq!(
                previous.lookup(old_root, page * 4096),
                before.get(&page).copied().unwrap_or(0)
            );
        }
    }
}

#[test]
fn no_op_edits_allocate_nothing_and_preserve_unowned_supervisor_entries() {
    let (mut original, root) = Memory::<3>::new();
    // Sv39 copies the upper supervisor half from the kernel root. Those
    // entries are not owned by the Native table pool and must never be walked.
    let supervisor = 0x8000_00cf;
    require_ok(original.write(root, 256, supervisor));
    let mut next = original.fork();
    {
        let mut editor = require_ok(Editor::new(root, &mut next));
        require_ok(editor.set(0x1000, 0));
        require_ok(editor.clear(0, 1 << 38));
        assert_eq!(editor.root(), root);
    }
    assert_eq!(next.copies, 0);
    let new_root = {
        let mut editor = require_ok(Editor::new(root, &mut next));
        require_ok(editor.set(0x1000, 0x123003));
        require_ok(editor.clear(0, 1 << 38));
        editor.root()
    };
    assert_eq!(next.pages.len(), 1);
    assert_eq!(require_ok(next.read(new_root, 256)), supervisor);
    assert_eq!(require_ok(original.read(root, 256)), supervisor);
    let mut editor = require_ok(Editor::new(new_root, &mut next));
    assert_eq!(
        editor.clear(u64::MAX - 4095, 4096),
        Err(Error::InvalidRange)
    );
    assert_eq!(editor.set(3, 0x1003), Err(Error::InvalidRange));
}
