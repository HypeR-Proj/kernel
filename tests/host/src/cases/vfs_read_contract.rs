// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::vfs_read_contract::Error;

#[test]
fn bulk_read_preserves_offsets_across_batches() {
    use crate::vfs_read_contract::read_batches;
    let mut sizes = Vec::new();
    let result = read_batches::<()>(17, 2 * 1024 * 1024, 65536, |offset, done, size| {
        assert_eq!(offset, 17 + done as u64);
        if !sizes.is_empty() {
            assert_eq!(offset % 65536, 0);
        }
        assert_eq!(offset / 65536, (offset + size as u64 - 1) / 65536);
        sizes.push(size);
        Ok(size)
    });
    assert_eq!(result, Ok(2 * 1024 * 1024));
    assert_eq!(sizes.len(), 33);
    assert_eq!(sizes[0], 65536 - 17);
    assert_eq!(sizes[32], 17);
    assert_eq!(sizes.iter().sum::<usize>(), 2 * 1024 * 1024);
}

#[test]
fn bulk_read_returns_only_delivered_prefix_on_failure_or_short_read() {
    use crate::vfs_read_contract::{BatchError, read_batches};
    assert_eq!(
        read_batches(0, 100, 16, |_, _, _| Err("fault")),
        Err(BatchError::Transfer("fault"))
    );
    assert_eq!(
        read_batches(0, 100, 16, |_, done, size| if done == 0 {
            Ok(size)
        } else {
            Err("fault")
        }),
        Ok(16)
    );
    let mut calls = 0;
    assert_eq!(
        read_batches::<()>(0, 100, 16, |_, done, size| {
            calls += 1;
            Ok(if done == 0 { size } else { 3 })
        }),
        Ok(19)
    );
    assert_eq!(calls, 2);
    assert_eq!(
        read_batches::<()>(0, 0, 16, |_, _, _| panic!("empty read")),
        Ok(0)
    );
}

#[test]
fn bulk_read_rejects_overflow_and_invalid_backend_lengths() {
    use crate::vfs_read_contract::{BatchError, read_batches};
    assert_eq!(
        read_batches::<()>(u64::MAX, 1, 16, |_, _, _| panic!("overflow")),
        Err(BatchError::Contract(Error::ArithmeticOverflow))
    );
    assert_eq!(
        read_batches::<()>(0, 100, 16, |_, _, size| Ok(size + 1)),
        Err(BatchError::Contract(Error::InvalidBackendResult))
    );
    assert_eq!(
        read_batches::<()>(0, 100, 0, |_, _, _| panic!("zero batch")),
        Err(BatchError::Contract(Error::InvalidBackendResult))
    );
}

#[test]
fn buffered_reply_scatters_one_512k_payload_across_128_pages() {
    use crate::vfs_read_contract::scatter_copy;
    let source: Vec<u8> = (0..512 * 1024).map(|index| (index % 251) as u8).collect();
    let mut pages = vec![vec![0; 4096]; 128];
    let mut targets: Vec<_> = pages.iter_mut().map(Vec::as_mut_slice).collect();
    let mut next = 0;
    assert_eq!(
        scatter_copy::<()>(&mut targets, 0, source.len(), |offset, output| {
            assert_eq!(offset, next);
            output.copy_from_slice(&source[offset..offset + output.len()]);
            next += output.len();
            Ok(())
        }),
        Ok(())
    );
    assert_eq!(next, source.len());
    assert!(pages.iter().flatten().copied().eq(source));
}

#[test]
fn buffered_scatter_preserves_planned_prefix_and_short_reply_tail() {
    use crate::vfs_read_contract::scatter_copy;
    let mut first = [0xa5; 7];
    let mut middle = [0xa5; 11];
    let mut last = [0xa5; 3];
    let mut empty = [];
    let source = [0x37; 13];
    assert_eq!(
        scatter_copy::<()>(
            &mut [&mut empty, &mut first, &mut middle, &mut last],
            5,
            source.len(),
            |offset, output| {
                output.copy_from_slice(&source[offset..offset + output.len()]);
                Ok(())
            },
        ),
        Ok(())
    );
    assert_eq!(&first[..5], &[0xa5; 5]);
    assert_eq!(&first[5..], &[0x37; 2]);
    assert_eq!(middle, [0x37; 11]);
    assert_eq!(last, [0xa5; 3]);
}

#[test]
fn buffered_scatter_rejects_bounds_before_copy_and_propagates_copy_fault() {
    use crate::vfs_read_contract::{BatchError, scatter_copy};
    let mut first = [0xa5; 4];
    let mut last = [0xa5; 4];
    for (skip, length) in [(9, 0), (7, 2), (usize::MAX, 1)] {
        assert_eq!(
            scatter_copy::<()>(&mut [&mut first, &mut last], skip, length, |_, _| {
                panic!("invalid range reached the source")
            }),
            Err(BatchError::Contract(Error::InvalidBackendResult))
        );
        assert_eq!(first, [0xa5; 4]);
        assert_eq!(last, [0xa5; 4]);
    }
    assert_eq!(
        scatter_copy(&mut [&mut first, &mut last], 0, 8, |offset, output| {
            if offset == 0 {
                output.fill(0x37);
                Ok(())
            } else {
                Err("copy fault")
            }
        }),
        Err(BatchError::Transfer("copy fault"))
    );
    assert_eq!(first, [0x37; 4]);
    assert_eq!(last, [0xa5; 4]);
    assert_eq!(
        scatter_copy::<()>(&mut [&mut first, &mut last], 8, 0, |_, _| {
            panic!("empty range accessed the source")
        }),
        Ok(())
    );
}

#[test]
fn small_crossing_reads_stay_whole_but_page_rounded_overflow_is_split() {
    use crate::vfs_read_contract::read_batches;
    const BATCH: usize = 512 * 1024;
    let mut calls = Vec::new();
    assert_eq!(
        read_batches::<()>(BATCH as u64 - 17, 4096, BATCH, |at, done, count| {
            calls.push((at, done, count));
            Ok(count)
        }),
        Ok(4096)
    );
    assert_eq!(calls, [(BATCH as u64 - 17, 0, 4096)]);
    calls.clear();
    assert_eq!(
        read_batches::<()>(13, BATCH - 1, BATCH, |at, done, count| {
            assert!((at % 4096) + count as u64 <= BATCH as u64);
            calls.push((at, done, count));
            Ok(count)
        }),
        Ok(BATCH - 1)
    );
    assert_eq!(calls, [(13, 0, BATCH - 13), (BATCH as u64, BATCH - 13, 12)]);
}
