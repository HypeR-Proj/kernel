// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selection and successful retirement of volume-ordered dirty page versions.

use alloc::vec::Vec;
use hyper_filesystem::Timestamp;

pub(super) struct Pending<Page> {
    pub(super) version: u64,
    pub(super) node: u64,
    pub(super) offset: u64,
    pub(super) page: Page,
    pub(super) modified: Option<Timestamp>,
}

pub(super) struct Batch {
    pub(super) first: usize,
    pub(super) end: usize,
    pub(super) node: u64,
    pub(super) offset: u64,
    pub(super) length: usize,
    pub(super) modified: Option<Timestamp>,
    pub(super) last: bool,
}

pub(super) fn modified<Page>(pages: &[Pending<Page>], node: u64) -> Option<Timestamp> {
    pages
        .iter()
        .filter(|page| page.node == node)
        .filter_map(|page| page.modified)
        .max_by_key(|time| (time.seconds(), time.nanoseconds()))
}

/// Pages are sorted by (node, offset). Rotate between files, retaining contiguous
/// order within one file. A selected file always starts at its first dirty page.
pub(super) fn select<Page>(
    pages: &[Pending<Page>],
    selected: Option<u64>,
    cursor: u64,
    maximum: usize,
    size: impl Fn(&Page) -> usize,
) -> Option<Batch> {
    let first = if let Some(node) = selected {
        let first = pages.partition_point(|page| page.node < node);
        if pages.get(first).is_none_or(|page| page.node != node) {
            return None;
        }
        first
    } else {
        let first = pages.partition_point(|page| page.node <= cursor);
        if first == pages.len() { 0 } else { first }
    };
    let start = pages.get(first)?;
    let (node, offset) = (start.node, start.offset);
    let mut length = 0;
    let mut end = first;
    while let Some(page) = pages.get(end) {
        let bytes = size(&page.page);
        if page.node != node
            || offset.checked_add(length as u64) != Some(page.offset)
            || bytes == 0
            || bytes > maximum - length
        {
            break;
        }
        length += bytes;
        end += 1;
    }
    (length != 0).then(|| Batch {
        first,
        end,
        node,
        offset,
        length,
        modified: modified(pages, node),
        last: pages.get(end).is_none_or(|page| page.node != node),
    })
}

/// Freeze a finite owner set without allocating. Later replacement happens in
/// pending, never in frozen, so continuous redirty cannot extend this cut.
pub(super) fn freeze<Page>(
    pending: &mut Vec<Pending<Page>>,
    frozen: &mut Vec<Pending<Page>>,
    selected: Option<u64>,
) {
    if selected.is_none() && frozen.is_empty() {
        core::mem::swap(pending, frozen);
        return;
    }
    let mut retained = 0;
    for index in 0..pending.len() {
        if selected.is_some_and(|node| pending[index].node != node) {
            pending.swap(retained, index);
            retained += 1;
        }
    }
    frozen.extend(pending.drain(retained..));
    frozen.sort_unstable_by_key(|p| (p.node, p.offset, p.version));
    pending.sort_unstable_by_key(|p| (p.node, p.offset));
}

/// Selection indices are valid only under State until this ownership transfer.
/// Completion drops the transferred owners, never indices into a mutable queue.
pub(super) fn take<Page>(
    pages: &mut Vec<Pending<Page>>,
    batch: &Batch,
    owners: &mut Vec<Pending<Page>>,
) {
    owners.extend(pages.drain(batch.first..batch.end));
    if let Some(next) = pages.get_mut(batch.first)
        && next.node == batch.node
    {
        next.modified = batch.modified;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(node: u64, offset: u64, seconds: i64) -> Pending<usize> {
        Pending {
            version: 0,
            node,
            offset,
            page: 4096,
            modified: Timestamp::new(seconds, 0),
        }
    }

    #[test]
    fn completed_prefix_retains_newest_timestamp_until_last_batch() {
        let mut pages = alloc::vec![page(1, 0, 30), page(1, 4096, 10), page(1, 8192, 20)];
        for last in [false, false, true] {
            let Some(batch) = select(&pages, Some(1), 0, 4096, |size| *size) else {
                panic!("missing dirty batch");
            };
            assert_eq!(batch.modified, Timestamp::new(30, 0));
            assert_eq!(batch.last, last);
            take(&mut pages, &batch, &mut Vec::new());
        }
        assert!(pages.is_empty());
    }

    #[test]
    fn selection_rotates_files_and_stops_at_gaps_and_byte_limit() {
        let mut pages = alloc::vec![
            page(1, 0, 1),
            page(1, 4096, 2),
            page(1, 12288, 3),
            page(2, 0, 4),
            page(2, 4096, 5),
        ];
        let Some(first) = select(&pages, None, 0, 8192, |size| *size) else {
            panic!("missing first batch");
        };
        assert_eq!((first.node, first.length, first.last), (1, 8192, false));
        take(&mut pages, &first, &mut Vec::new());
        let Some(second) = select(&pages, None, first.node, 4096, |size| *size) else {
            panic!("missing next file");
        };
        assert_eq!((second.node, second.length, second.last), (2, 4096, false));
        take(&mut pages, &second, &mut Vec::new());
        let Some(third) = select(&pages, None, second.node, 8192, |size| *size) else {
            panic!("missing wrapped file");
        };
        assert_eq!((third.node, third.offset, third.last), (1, 12288, true));
        assert!(select(&pages, Some(3), 0, 8192, |size| *size).is_none());
    }

    #[test]
    fn timestamp_carry_is_isolated_from_other_files_and_newer_writes() {
        let mut pages = alloc::vec![page(1, 0, 30), page(1, 4096, 10), page(2, 0, 5)];
        let Some(first) = select(&pages, None, 0, 4096, |size| *size) else {
            panic!("missing first file");
        };
        take(&mut pages, &first, &mut Vec::new());
        let Some(other) = select(&pages, None, first.node, 4096, |size| *size) else {
            panic!("missing other file");
        };
        assert_eq!(other.modified, Timestamp::new(5, 0));
        take(&mut pages, &other, &mut Vec::new());
        pages.push(page(1, 8192, 40));
        let Some(next) = select(&pages, Some(1), 0, 4096, |size| *size) else {
            panic!("missing remaining file");
        };
        assert_eq!(next.modified, Timestamp::new(40, 0));
        take(&mut pages, &next, &mut Vec::new());
        assert_eq!(modified(&pages, 1), Timestamp::new(40, 0));
    }
    #[test]
    fn frozen_cut_retains_old_version_while_new_writes_continue() {
        let mut old = page(1, 0, 10);
        old.version = 1;
        let mut pending = Vec::with_capacity(8);
        pending.push(old);
        let mut frozen = Vec::with_capacity(8);
        freeze(&mut pending, &mut frozen, None);
        let old_buffer = frozen.as_ptr();
        for version in 2..20 {
            pending.clear();
            let mut latest = page(1, 0, version as i64);
            latest.version = version;
            pending.push(latest);
        }
        let Some(batch) = select(&frozen, None, 0, 512 * 1024, |n| *n) else {
            panic!("expected selected dirty batch");
        };
        let mut accepted = Vec::with_capacity(128);
        take(&mut frozen, &batch, &mut accepted);
        assert_eq!(accepted[0].version, 1);
        assert_eq!(pending[0].version, 19);
        assert!(frozen.is_empty());
        assert_eq!(frozen.as_ptr(), old_buffer);
        drop(accepted); // Completing the old batch never removes the new version.
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn selected_cut_merges_older_frozen_versions_without_reordering_overwrites() {
        let mut pending = Vec::with_capacity(8);
        for (node, offset, version) in [(1, 0, 3), (1, 4096, 4), (2, 0, 5), (3, 0, 6)] {
            let mut p = page(node, offset, version as i64);
            p.version = version;
            pending.push(p);
        }
        let mut frozen = Vec::with_capacity(8);
        let mut old = page(1, 0, 1);
        old.version = 1;
        frozen.push(old);
        let before = (pending.capacity(), frozen.capacity());
        freeze(&mut pending, &mut frozen, Some(1));
        assert_eq!(pending.iter().map(|p| p.node).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(
            frozen.iter().map(|p| p.version).collect::<Vec<_>>(),
            [1, 3, 4]
        );
        let Some(first) = select(&frozen, Some(1), 0, 8192, |n| *n) else {
            panic!("expected selected dirty batch");
        };
        assert_eq!(first.length, 4096); // Overlap starts another ordered batch.
        let mut accepted = Vec::with_capacity(128);
        take(&mut frozen, &first, &mut accepted);
        let Some(second) = select(&frozen, Some(1), 0, 8192, |n| *n) else {
            panic!("expected selected dirty batch");
        };
        assert_eq!(
            (second.length, second.modified),
            (8192, Timestamp::new(4, 0))
        );
        assert_eq!((pending.capacity(), frozen.capacity()), before);
    }
}
