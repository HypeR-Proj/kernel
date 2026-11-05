// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selection and successful retirement of volume-ordered dirty page versions.

use alloc::vec::Vec;
use hyper_filesystem::Timestamp;

pub(super) struct Pending<Page> {
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

/// Caller retains the volume mutex from selection through successful backend
/// completion. The last batch also requires `SetTimes` success before retirement.
pub(super) fn retire<Page>(pages: &mut Vec<Pending<Page>>, batch: &Batch) {
    // Preserve the newest pending timestamp even when its original page was
    // submitted before the remaining, older pages. No metadata-only lease is
    // needed: the last page stays pinned until its SetTimes is acknowledged.
    if let Some(next) = pages.get_mut(batch.end)
        && next.node == batch.node
    {
        next.modified = batch.modified;
    }
    pages.drain(batch.first..batch.end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(node: u64, offset: u64, seconds: i64) -> Pending<usize> {
        Pending {
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
            retire(&mut pages, &batch);
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
        retire(&mut pages, &first);
        let Some(second) = select(&pages, None, first.node, 4096, |size| *size) else {
            panic!("missing next file");
        };
        assert_eq!((second.node, second.length, second.last), (2, 4096, false));
        retire(&mut pages, &second);
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
        retire(&mut pages, &first);
        let Some(other) = select(&pages, None, first.node, 4096, |size| *size) else {
            panic!("missing other file");
        };
        assert_eq!(other.modified, Timestamp::new(5, 0));
        retire(&mut pages, &other);
        pages.push(page(1, 8192, 40));
        let Some(next) = select(&pages, Some(1), 0, 4096, |size| *size) else {
            panic!("missing remaining file");
        };
        assert_eq!(next.modified, Timestamp::new(40, 0));
        retire(&mut pages, &next);
        assert_eq!(modified(&pages, 1), Timestamp::new(40, 0));
    }
}
