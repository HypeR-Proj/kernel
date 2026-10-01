// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::num::NonZeroU64;
use core::ops::Range;

use crate::file_data_cache::{
    CacheAccess, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePage, FilePageIndex,
    FilesystemGeneration, NodeIdentity, ReadError, read,
};

const PAGE: usize = 4096;
const BATCH: usize = 512 * 1024;

fn identity(filesystem: u64, node: u64, revision: u64) -> FileIdentity {
    FileIdentity::new(
        FilesystemGeneration::new(crate::require_some(NonZeroU64::new(filesystem))),
        NodeIdentity::new(crate::require_some(NonZeroU64::new(node))),
        ContentRevision::new(crate::require_some(NonZeroU64::new(revision))),
    )
}

fn pattern(offset: u64) -> u8 {
    (offset.wrapping_mul(37) ^ (offset >> 8)) as u8
}

/// Resident backend: complete reads with independently owned mutable contents.
struct MemoryFile {
    bytes: Vec<u8>,
    reads: Vec<(u64, usize)>,
}

impl MemoryFile {
    fn new(length: usize) -> Self {
        Self {
            bytes: (0..length).map(|offset| pattern(offset as u64)).collect(),
            reads: Vec::new(),
        }
    }

    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, &'static str> {
        self.reads.push((offset, output.len()));
        let available = self.bytes.get(offset as usize..).ok_or("offset")?;
        let actual = output.len().min(available.len());
        output[..actual].copy_from_slice(&available[..actual]);
        Ok(actual)
    }
}

/// A different backend model: bounded short transfers, generated contents, and
/// irreversible failure on any read outside the caller's admitted range.
struct RangeDevice {
    allowed: Range<u64>,
    max_read: usize,
    fail_call: Option<usize>,
    poisoned: bool,
    reads: Vec<(u64, usize)>,
}

impl RangeDevice {
    fn new(allowed: Range<u64>, max_read: usize) -> Self {
        Self {
            allowed,
            max_read,
            fail_call: None,
            poisoned: false,
            reads: Vec::new(),
        }
    }

    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, &'static str> {
        self.reads.push((offset, output.len()));
        if self.poisoned
            || offset < self.allowed.start
            || offset
                .checked_add(output.len() as u64)
                .is_none_or(|end| end > self.allowed.end)
            || self.fail_call == Some(self.reads.len())
        {
            self.poisoned = true;
            return Err("device failed");
        }
        let actual = output.len().min(self.max_read);
        for (index, byte) in output[..actual].iter_mut().enumerate() {
            *byte = pattern(offset + index as u64);
        }
        Ok(actual)
    }
}

fn read_memory(
    cache: &FileDataCache<FilePage>,
    file: FileIdentity,
    source: &mut MemoryFile,
    offset: u64,
    output: &mut [u8],
) -> usize {
    crate::require_ok(read(
        cache,
        file,
        &(),
        source.bytes.len() as u64,
        offset,
        output,
        |offset, bytes| source.read_at(offset, bytes),
    ))
}

#[test]
fn cold_bulk_uses_one_backend_read_and_warm_subranges_hit() {
    let cache = crate::require_ok(FileDataCache::try_new(128));
    let file = identity(1, 1, 1);
    let mut source = MemoryFile::new(BATCH);
    let mut output = vec![0; BATCH];
    assert_eq!(
        read_memory(&cache, file, &mut source, 0, &mut output),
        BATCH
    );
    assert_eq!(output, source.bytes);
    assert_eq!(source.reads, vec![(0, BATCH)]);
    assert_eq!(cache.snapshot().live_payloads, 128);
    output.fill(0);
    assert_eq!(
        read_memory(&cache, file, &mut source, 0, &mut output),
        BATCH
    );
    assert_eq!(output, source.bytes);
    let mut small = [0; 37];
    assert_eq!(
        read_memory(&cache, file, &mut source, 17, &mut small),
        small.len()
    );
    assert_eq!(small, source.bytes[17..54]);
    assert_eq!(source.reads.len(), 1);
}

#[test]
fn cold_reads_preserve_the_existing_maximum_batch_size() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let mut source = MemoryFile::new(2 * BATCH + 37);
    let mut output = vec![0; source.bytes.len()];
    assert_eq!(
        read_memory(&cache, identity(1, 2, 1), &mut source, 0, &mut output),
        output.len()
    );
    assert_eq!(output, source.bytes);
    assert_eq!(
        source.reads,
        vec![(0, BATCH), (BATCH as u64, BATCH), ((2 * BATCH) as u64, 37)]
    );
    assert!(cache.snapshot().live_payloads <= 2);
}

#[test]
fn repeated_scans_reuse_payloads_without_fragmenting_reads_into_pages() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let file = identity(2, 1, 1);
    let mut source = MemoryFile::new(BATCH);
    let mut output = vec![0; BATCH];
    for scan in 0..3 {
        source.reads.clear();
        output.fill(0);
        assert_eq!(
            read_memory(&cache, file, &mut source, 0, &mut output),
            BATCH
        );
        assert_eq!(output, source.bytes);
        if scan == 0 {
            assert_eq!(source.reads, vec![(0, BATCH)]);
        } else {
            // The warm tail initially bounds the miss, but admitting the
            // preceding pages can evict it before the next lookup. That may
            // require a second read; it must not produce per-page requests.
            assert!(source.reads.len() <= 2);
            let mut next = 0;
            for &(offset, count) in &source.reads {
                assert_eq!(offset, next);
                assert!((2 * PAGE..=BATCH).contains(&count));
                next += count as u64;
            }
            assert_eq!(next, BATCH as u64);
        }
        let snapshot = cache.snapshot();
        // These counters measure payload construction and allocation-free
        // refill on the actual read path, rather than allocator address reuse.
        assert_eq!(snapshot.payload_creations, 2);
        assert_eq!(snapshot.payload_reuses, (scan + 1) * (BATCH / PAGE) - 2);
        assert_eq!(snapshot.live_payloads, 2);
        assert_eq!(snapshot.loading, 0);
    }
}

#[test]
fn recycled_eof_tails_reuse_the_same_complete_page_backing() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let mut source = MemoryFile::new(37);
    let mut output = vec![0xcc; PAGE];
    for (index, (length, value, creations, reuses)) in [
        (37, 0x31, 1, 0),
        (PAGE, 0x52, 1, 1),
        (37, 0x73, 1, 2),
        (PAGE, 0x94, 1, 3),
    ]
    .into_iter()
    .enumerate()
    {
        source.bytes.resize(length, value);
        source.bytes.fill(value);
        source.reads.clear();
        output.fill(0xcc);
        assert_eq!(
            read_memory(
                &cache,
                identity(2, 2, index as u64 + 1),
                &mut source,
                0,
                &mut output
            ),
            length
        );
        assert!(output[..length].iter().all(|&byte| byte == value));
        assert!(output[length..].iter().all(|&byte| byte == 0xcc));
        assert_eq!(source.reads, vec![(0, length)]);
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.payload_creations, creations);
        assert_eq!(snapshot.payload_reuses, reuses);
        assert_eq!(snapshot.live_payloads, 1);
        assert_eq!(snapshot.loading, 0);
    }
}

#[test]
fn pinned_payload_stays_immutable_while_bulk_reads_bypass_full_budget() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let first = identity(2, 3, 1);
    let mut old_source = MemoryFile::new(PAGE);
    old_source.bytes.fill(0x31);
    let mut warm = vec![0; PAGE];
    assert_eq!(
        read_memory(&cache, first, &mut old_source, 0, &mut warm),
        PAGE
    );
    let pinned = crate::require_some(crate::require_ok(
        cache.lookup(CacheKey::new(first, FilePageIndex::new(0))),
    ));
    let mut source = MemoryFile::new(BATCH);
    let mut output = vec![0; BATCH];
    let second = identity(2, 4, 1);
    assert_eq!(
        read_memory(&cache, second, &mut source, 0, &mut output),
        BATCH
    );
    assert_eq!(output, source.bytes);
    assert_eq!(source.reads, vec![(0, BATCH)]);
    assert_eq!(pinned.value().bytes(), &old_source.bytes);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.payload_creations, 1);
    assert_eq!(snapshot.payload_reuses, 0);
    assert_eq!(snapshot.live_payloads, 1);
    assert_eq!((snapshot.clean, snapshot.loading), (0, 0));

    drop(pinned);
    assert_eq!(cache.snapshot().live_payloads, 0);
    source.reads.clear();
    output.fill(0);
    assert_eq!(
        read_memory(&cache, second, &mut source, 0, &mut output),
        BATCH
    );
    assert_eq!(output, source.bytes);
    assert_eq!(source.reads, vec![(0, BATCH)]);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.payload_creations, 2);
    assert_eq!(snapshot.payload_reuses, BATCH / PAGE - 1);
    assert_eq!(snapshot.live_payloads, 1);
}

#[test]
fn failed_recycled_publication_releases_budget_without_failing_the_read() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let mut source = MemoryFile::new(PAGE);
    let mut output = vec![0; PAGE];
    read_memory(&cache, identity(2, 5, 1), &mut source, 0, &mut output);
    source.bytes.fill(0x72);
    source.reads.clear();
    cache.fail_next_publication_for_test();
    let file = identity(2, 5, 2);
    assert_eq!(read_memory(&cache, file, &mut source, 0, &mut output), PAGE);
    assert_eq!(output, source.bytes);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.payload_creations, 1);
    assert_eq!(snapshot.payload_reuses, 1);
    assert_eq!(snapshot.live_payloads, 0);
    assert_eq!((snapshot.clean, snapshot.loading), (0, 0));

    // Failed retention leaves the slot and its permit available. The retry
    // creates a replacement; the following warm read must need no backend I/O.
    for _ in 0..2 {
        output.fill(0);
        assert_eq!(read_memory(&cache, file, &mut source, 0, &mut output), PAGE);
        assert_eq!(output, source.bytes);
    }
    assert_eq!(source.reads, vec![(0, PAGE), (0, PAGE)]);
    let snapshot = cache.snapshot();
    assert_eq!(snapshot.payload_creations, 2);
    assert_eq!(snapshot.payload_reuses, 1);
    assert_eq!(snapshot.live_payloads, 1);
    assert_eq!((snapshot.clean, snapshot.loading), (1, 0));
}

#[test]
fn cached_and_pinned_pages_retain_the_generic_file_owner() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let owner = std::sync::Arc::new(17_u64);
    let weak = std::sync::Arc::downgrade(&owner);
    let file = identity(11, 7, 1);
    let mut source = MemoryFile::new(PAGE);
    let mut output = vec![0; PAGE];
    assert_eq!(
        crate::require_ok(read(
            &cache,
            file,
            &owner,
            PAGE as u64,
            0,
            &mut output,
            |offset, bytes| source.read_at(offset, bytes),
        )),
        PAGE
    );
    assert_eq!(std::sync::Arc::strong_count(&owner), 2);
    let pin = crate::require_some(crate::require_ok(
        cache.lookup(CacheKey::new(file, FilePageIndex::new(0))),
    ));
    // Cloning a cached payload does not make another file-record owner.
    assert_eq!(std::sync::Arc::strong_count(&owner), 2);
    drop(owner);
    assert!(weak.upgrade().is_some());
    assert_eq!(cache.reclaim_batch(64), 1);
    assert!(weak.upgrade().is_some());
    assert_eq!(pin.value().bytes(), source.bytes);
    drop(pin);
    assert!(weak.upgrade().is_none());
    assert_eq!(cache.usage().live_payloads, 0);
}

#[test]
fn pressure_bypass_preserves_exact_io_without_retaining_an_owner() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let owner = std::sync::Arc::new(23_u64);
    let mut source = MemoryFile::new(BATCH);
    let mut output = vec![0; BATCH];
    cache.set_admission(false);
    assert_eq!(
        crate::require_ok(read(
            &cache,
            identity(12, 8, 1),
            &owner,
            BATCH as u64,
            0,
            &mut output,
            |offset, bytes| source.read_at(offset, bytes),
        )),
        BATCH
    );
    assert_eq!(output, source.bytes);
    assert_eq!(source.reads, vec![(0, BATCH)]);
    assert_eq!(std::sync::Arc::strong_count(&owner), 1);
    assert_eq!(cache.usage().clean, 0);
    assert_eq!(cache.usage().live_payloads, 0);
}

#[test]
fn partial_miss_never_reads_outside_the_requested_range_or_retains_padding() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let mut source = RangeDevice::new(100..137, usize::MAX);
    let mut output = [0; 37];
    for _ in 0..2 {
        assert_eq!(
            crate::require_ok(read(
                &cache,
                identity(1, 3, 1),
                &(),
                PAGE as u64,
                100,
                &mut output,
                |offset, bytes| source.read_at(offset, bytes)
            )),
            37
        );
        assert_eq!(
            output,
            core::array::from_fn::<_, 37, _>(|index| pattern(100 + index as u64))
        );
    }
    assert!(!source.poisoned);
    assert_eq!(source.reads, vec![(100, 37), (100, 37)]);
    assert_eq!(cache.snapshot().live_payloads, 0);
    assert_eq!(cache.snapshot().misses, 0);
}

#[test]
fn unaligned_bulk_admits_only_pages_present_at_their_full_logical_offsets() {
    let cache = crate::require_ok(FileDataCache::try_new(8));
    let file = identity(1, 4, 1);
    let mut source = MemoryFile::new(4 * PAGE);
    let mut output = vec![0; 3 * PAGE];
    assert_eq!(
        read_memory(&cache, file, &mut source, 7, &mut output),
        output.len()
    );
    assert_eq!(output, source.bytes[7..7 + output.len()]);
    assert_eq!(source.reads, vec![(7, 3 * PAGE)]);
    assert!(crate::require_ok(cache.lookup(CacheKey::new(file, FilePageIndex::new(0)))).is_none());
    assert!(crate::require_ok(cache.lookup(CacheKey::new(file, FilePageIndex::new(3)))).is_none());
    assert_eq!(cache.snapshot().clean, 2);
    let mut prefix = [0; 13];
    assert_eq!(read_memory(&cache, file, &mut source, 0, &mut prefix), 13);
    assert_eq!(prefix, source.bytes[..13]);
    assert_eq!(source.reads.last(), Some(&(0, 13)));
}

#[test]
fn known_eof_tail_is_cached_but_short_non_eof_prefix_is_not() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let file = identity(1, 5, 1);
    let mut source = MemoryFile::new(PAGE + 37);
    let mut tail = [0; 100];
    assert_eq!(
        read_memory(&cache, file, &mut source, PAGE as u64, &mut tail),
        37
    );
    assert_eq!(source.reads, vec![(PAGE as u64, 37)]);
    assert_eq!(
        read_memory(&cache, file, &mut source, PAGE as u64 + 30, &mut tail),
        7
    );
    assert_eq!(&tail[..7], &source.bytes[PAGE + 30..]);
    assert_eq!(source.reads.len(), 1);

    let other = identity(1, 6, 1);
    let mut short = RangeDevice::new(0..(3 * PAGE) as u64, PAGE + 31);
    let mut output = vec![0; 3 * PAGE];
    assert_eq!(
        crate::require_ok(read(
            &cache,
            other,
            &(),
            output.len() as u64,
            0,
            &mut output,
            |offset, bytes| short.read_at(offset, bytes)
        )),
        PAGE + 31
    );
    assert!(crate::require_ok(cache.lookup(CacheKey::new(other, FilePageIndex::new(0)))).is_some());
    assert!(crate::require_ok(cache.lookup(CacheKey::new(other, FilePageIndex::new(1)))).is_none());
    assert_eq!(
        crate::require_ok(read(
            &cache,
            other,
            &(),
            (3 * PAGE) as u64,
            PAGE as u64 + 100,
            &mut tail,
            |offset, bytes| short.read_at(offset, bytes)
        )),
        100
    );
    assert_eq!(short.reads.last(), Some(&(PAGE as u64 + 100, 100)));
}

#[test]
fn mixed_hits_and_misses_keep_adjacent_misses_coalesced() {
    let cache = crate::require_ok(FileDataCache::try_new(16));
    let file = identity(1, 7, 1);
    let mut source = MemoryFile::new(12 * PAGE);
    let mut middle = vec![0; 4 * PAGE];
    read_memory(&cache, file, &mut source, (4 * PAGE) as u64, &mut middle);
    source.reads.clear();
    let mut output = vec![0; 12 * PAGE];
    assert_eq!(
        read_memory(&cache, file, &mut source, 0, &mut output),
        output.len()
    );
    assert_eq!(output, source.bytes);
    assert_eq!(
        source.reads,
        vec![(0, 4 * PAGE), ((8 * PAGE) as u64, 4 * PAGE)]
    );
}

#[test]
fn revisions_node_incarnations_and_filesystem_generations_isolate_contents() {
    let cache = crate::require_ok(FileDataCache::try_new(8));
    let mut source = MemoryFile::new(PAGE);
    let mut output = vec![0; PAGE];
    for (index, file) in [
        identity(1, 1, 1),
        identity(1, 1, 2),
        identity(1, 2, 1),
        identity(2, 1, 1),
    ]
    .into_iter()
    .enumerate()
    {
        source.bytes.fill(index as u8);
        assert_eq!(read_memory(&cache, file, &mut source, 0, &mut output), PAGE);
        assert!(output.iter().all(|&byte| byte == index as u8));
    }
    assert_eq!(source.reads.len(), 4);
    read_memory(&cache, identity(1, 1, 1), &mut source, 0, &mut output);
    assert!(output.iter().all(|&byte| byte == 0));
    assert_eq!(source.reads.len(), 4);
}

#[test]
fn publication_failure_and_sequence_exhaustion_do_not_fail_successful_reads() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let mut source = MemoryFile::new(PAGE);
    let mut output = vec![0; PAGE];
    cache.fail_next_publication_for_test();
    assert_eq!(
        read_memory(&cache, identity(1, 8, 1), &mut source, 0, &mut output),
        PAGE
    );
    assert_eq!(output, source.bytes);
    assert_eq!(cache.snapshot().live_payloads, 0);
    cache.exhaust_sequences_for_test();
    assert_eq!(
        read_memory(&cache, identity(1, 8, 1), &mut source, 0, &mut output),
        PAGE
    );
    assert_eq!(output, source.bytes);
    assert_eq!(cache.snapshot().loading, 0);
    assert_eq!(cache.snapshot().live_payloads, 0);
}

#[test]
fn fully_reserved_cache_bypasses_retention_without_fragmenting_io() {
    let cache = crate::require_ok(FileDataCache::try_new(1));
    let reserved = match crate::require_ok(
        cache.access(CacheKey::new(identity(1, 9, 1), FilePageIndex::new(0))),
    ) {
        CacheAccess::Load(load) => load,
        _ => panic!("expected a fresh reservation"),
    };
    let mut source = MemoryFile::new(BATCH);
    let mut output = vec![0; BATCH];
    assert_eq!(
        read_memory(&cache, identity(1, 10, 1), &mut source, 0, &mut output),
        BATCH
    );
    assert_eq!(output, source.bytes);
    assert_eq!(source.reads, vec![(0, BATCH)]);
    assert_eq!(cache.snapshot().live_payloads, 1);
    drop(reserved);
    assert_eq!(cache.snapshot().live_payloads, 0);
}

#[test]
fn backend_errors_propagate_even_after_populating_kernel_scratch() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let file = identity(1, 11, 1);
    let mut source = RangeDevice::new(0..(2 * BATCH) as u64, usize::MAX);
    source.fail_call = Some(2);
    let mut output = vec![0; 2 * BATCH];
    assert_eq!(
        read(
            &cache,
            file,
            &(),
            output.len() as u64,
            0,
            &mut output,
            |offset, bytes| source.read_at(offset, bytes)
        ),
        Err(ReadError::Backend("device failed"))
    );
    assert!(source.poisoned);
    assert!(
        output[..BATCH]
            .iter()
            .enumerate()
            .all(|(offset, &byte)| byte == pattern(offset as u64))
    );
    assert_eq!(
        read(
            &cache,
            identity(1, 12, 1),
            &(),
            output.len() as u64,
            0,
            &mut output,
            |offset, bytes| source.read_at(offset, bytes)
        ),
        Err(ReadError::Backend("device failed"))
    );
    assert_eq!(
        read::<(), _>(
            &cache,
            identity(1, 13, 1),
            &(),
            output.len() as u64,
            0,
            &mut output,
            |_, bytes| Ok(bytes.len() + 1)
        ),
        Err(ReadError::InvalidBackendResult)
    );
}

#[test]
fn cached_prefix_does_not_hide_a_later_backend_failure() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let file = identity(1, 15, 1);
    let mut resident = MemoryFile::new(2 * PAGE);
    let mut warm = vec![0; PAGE];
    read_memory(&cache, file, &mut resident, 0, &mut warm);
    let mut failing = RangeDevice::new(PAGE as u64..(2 * PAGE) as u64, usize::MAX);
    failing.fail_call = Some(1);
    let mut output = vec![0; 2 * PAGE];
    assert_eq!(
        read(
            &cache,
            file,
            &(),
            output.len() as u64,
            0,
            &mut output,
            |offset, bytes| failing.read_at(offset, bytes)
        ),
        Err(ReadError::Backend("device failed"))
    );
    assert_eq!(&output[..PAGE], &warm);
    assert_eq!(failing.reads, vec![(PAGE as u64, PAGE)]);
    assert!(failing.poisoned);
    // Only the Native transfer layer can turn errors into an already-delivered
    // user prefix; a caller must not copy this failed batch's scratch buffer.
    assert_eq!(
        read::<(), _>(
            &cache,
            file,
            &(),
            output.len() as u64,
            0,
            &mut output,
            |_, bytes| Ok(bytes.len() + 1)
        ),
        Err(ReadError::InvalidBackendResult)
    );
}

#[test]
fn empty_eof_overflow_and_zero_backend_reads_never_publish_a_page() {
    let cache = crate::require_ok(FileDataCache::try_new(2));
    let file = identity(1, 14, 1);
    let mut output = [0; 37];
    assert_eq!(
        read::<(), _>(&cache, file, &(), 0, 0, &mut output, |_, _| panic!(
            "EOF backend call"
        )),
        Ok(0)
    );
    assert_eq!(
        read::<(), _>(&cache, file, &(), PAGE as u64, 0, &mut [], |_, _| panic!(
            "empty backend call"
        )),
        Ok(0)
    );
    assert_eq!(
        read::<(), _>(
            &cache,
            file,
            &(),
            u64::MAX,
            u64::MAX,
            &mut output,
            |_, _| panic!("overflow backend call")
        ),
        Err(ReadError::ArithmeticOverflow)
    );
    assert_eq!(
        read::<(), _>(
            &cache,
            file,
            &(),
            PAGE as u64,
            100,
            &mut output,
            |offset, bytes| {
                assert_eq!((offset, bytes.len()), (100, 37));
                Ok(0)
            }
        ),
        Ok(0)
    );
    assert_eq!(cache.snapshot().live_payloads, 0);
}
