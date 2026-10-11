// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::num::NonZeroU64;

use crate::file_data_cache::read_ahead::{MAX_WINDOW, Range, SequentialReader};
use crate::file_data_cache::{
    ContentRevision, FileDataCache, FileIdentity, FilePage, FilesystemGeneration, NodeIdentity,
    read_observed,
};

fn identity(revision: u64) -> FileIdentity {
    FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MIN),
        NodeIdentity::new(NonZeroU64::MIN),
        ContentRevision::new(crate::require_some(NonZeroU64::new(revision))),
    )
}

#[test]
fn hot_reopened_stream_does_not_prefetch() {
    let mut reader = SequentialReader::new();
    for offset in (0..8 * MAX_WINDOW).step_by(MAX_WINDOW) {
        assert_eq!(
            reader.observe(
                identity(1),
                (8 * MAX_WINDOW) as u64,
                offset as u64,
                MAX_WINDOW,
                false
            ),
            None
        );
    }
}

#[test]
fn small_random_reads_do_not_prefetch() {
    let mut reader = SequentialReader::new();
    for offset in [65536, 0, 98304, 4096, 1048576] {
        assert_eq!(
            reader.observe(identity(1), 2097152, offset, 4096, true),
            None
        );
    }
}

#[test]
fn sequential_misses_and_consumed_prefetch_advance_window() {
    let mut reader = SequentialReader::new();
    assert_eq!(reader.observe(identity(1), 1048576, 0, 4096, true), None);
    let first = crate::require_some(reader.observe(identity(1), 1048576, 4096, 4096, true));
    assert_eq!(
        first,
        Range {
            offset: 8192,
            length: 32768
        }
    );
    assert_eq!(
        reader.observe(identity(1), 1048576, 8192, 4096, false),
        None
    );
    assert_eq!(
        reader.observe(identity(1), 1048576, 12288, 4096, false),
        None
    );
    let next = crate::require_some(reader.observe(identity(1), 1048576, 16384, 8192, false));
    assert_eq!(next.offset, first.offset + first.length as u64);
    assert_eq!(next.length, 65536);
}

#[test]
fn revision_change_and_seek_reset_prediction() {
    let mut reader = SequentialReader::new();
    assert!(
        reader
            .observe(identity(1), 2097152, 0, 65536, true)
            .is_some()
    );
    assert_eq!(
        reader.observe(identity(2), 2097152, 65536, 4096, false),
        None
    );
    assert_eq!(
        reader.observe(identity(2), 2097152, 69632, 4096, false),
        None
    );
    assert_eq!(reader.observe(identity(2), 2097152, 0, 4096, true), None);
}

#[test]
fn windows_are_page_aligned_bounded_and_stop_at_eof() {
    let mut reader = SequentialReader::new();
    let eof = 4 * MAX_WINDOW as u64 + 123;
    let first = crate::require_some(reader.observe(identity(1), eof, 3, 65536, true));
    assert!(first.offset.is_multiple_of(4096));
    assert!(first.length <= MAX_WINDOW);
    let mut offset = 65539;
    while offset < eof {
        let count = (eof - offset).min(65536) as usize;
        if let Some(range) = reader.observe(identity(1), eof, offset, count, true) {
            assert!(range.offset.is_multiple_of(4096));
            assert!(range.length <= MAX_WINDOW);
            assert!(range.offset + range.length as u64 <= eof);
        }
        offset += count as u64;
    }
    assert_eq!(reader.observe(identity(1), eof, eof, 0, true), None);
    assert_eq!(
        reader.observe(identity(1), u64::MAX, u64::MAX, 1, true),
        None
    );
}

#[test]
fn observation_distinguishes_cache_hits_without_extra_backend_reads() {
    let cache = crate::require_ok(FileDataCache::<FilePage>::try_new(4));
    let mut bytes = [0_u8; 4096];
    let cold = crate::require_ok(read_observed(
        &cache,
        identity(1),
        &(),
        4096,
        0,
        &mut bytes,
        |_, output| {
            output.fill(7);
            Ok::<_, ()>(output.len())
        },
    ));
    assert_eq!(cold, (4096, true));
    let hot = crate::require_ok(read_observed(
        &cache,
        identity(1),
        &(),
        4096,
        0,
        &mut bytes,
        |_, _| Err("hot read must not visit backend"),
    ));
    assert_eq!(hot, (4096, false));
    assert!(bytes.iter().all(|&byte| byte == 7));
}
