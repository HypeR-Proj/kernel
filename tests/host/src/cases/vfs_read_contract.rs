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
