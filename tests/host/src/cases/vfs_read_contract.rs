// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::mm::PAGE_SIZE;

use crate::vfs_read_contract::{Error, ReadPlan, ReadProgress};

const PAGE_BYTES: usize = PAGE_SIZE as usize;

fn require_plan(offset: u64, file_len: u64, output: usize) -> ReadPlan {
    crate::require_some(crate::require_ok(ReadPlan::next(offset, file_len, output)))
}

#[test]
fn planning_stops_at_page_boundaries_and_eof() {
    let first = require_plan(PAGE_SIZE - 6, PAGE_SIZE + 904, 1024);
    assert_eq!(first.offset(), PAGE_SIZE - 6);
    assert_eq!(first.page_index(), 0);
    assert_eq!(first.page_offset(), 0);
    assert_eq!(first.within_page(), PAGE_BYTES - 6);
    assert_eq!(first.output_len(), 6);

    let last = require_plan(PAGE_SIZE, PAGE_SIZE + 904, 1024);
    assert_eq!(last.offset(), PAGE_SIZE);
    assert_eq!(last.page_index(), 1);
    assert_eq!(last.page_offset(), PAGE_SIZE);
    assert_eq!(last.within_page(), 0);
    assert_eq!(last.output_len(), 904);

    assert_eq!(
        crate::require_ok(ReadPlan::next(PAGE_SIZE + 904, PAGE_SIZE + 904, 1)),
        None
    );
    assert_eq!(crate::require_ok(ReadPlan::next(0, 1, 0)), None);
}

#[test]
fn backend_counts_cannot_exceed_the_planned_destination() {
    let plan = require_plan(0, PAGE_SIZE * 2, 1024);
    assert_eq!(plan.validate_read(1024), Ok(ReadProgress::Complete));
    assert_eq!(plan.validate_read(1025), Err(Error::InvalidBackendResult));
}

#[test]
fn partial_backend_reads_are_valid_and_terminate_the_operation() {
    let plan = require_plan(100, PAGE_SIZE, 512);
    assert_eq!(plan.validate_read(127), Ok(ReadProgress::Partial));
    assert_eq!(plan.validate_read(0), Ok(ReadProgress::Partial));
}

#[test]
fn cache_fill_requires_the_complete_file_page() {
    let full = require_plan(7, PAGE_SIZE * 2, 32);
    assert_eq!(full.validate_fill(PAGE_BYTES, PAGE_BYTES), Ok(()));
    assert_eq!(
        full.validate_fill(PAGE_BYTES - 1, PAGE_BYTES),
        Err(Error::InvalidBackendResult)
    );

    let tail = require_plan(PAGE_SIZE, PAGE_SIZE + 37, 32);
    assert_eq!(tail.validate_fill(37, PAGE_BYTES), Ok(()));
    assert_eq!(
        tail.validate_fill(38, PAGE_BYTES),
        Err(Error::InvalidBackendResult)
    );
    assert_eq!(tail.validate_fill(37, 36), Err(Error::InvalidBackendResult));
}

#[test]
fn bulk_read_preserves_offsets_across_batches() {
    use crate::vfs_read_contract::read_batches;
    let mut calls = 0;
    let result = read_batches::<()>(17, 2 * 1024 * 1024, 65536, |offset, done, size| {
        assert_eq!(offset, 17 + done as u64);
        assert_eq!(done, calls * 65536);
        assert_eq!(size, 65536);
        calls += 1;
        Ok(size)
    });
    assert_eq!(result, Ok(2 * 1024 * 1024));
    assert_eq!(calls, 32);
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
