// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::fs::primitives::file_data::{Error, FileData, PAGE_BYTES, Page, Storage, StorageBudget};
use hyper::mm::AllocationError;
use std::cell::Cell;
use std::rc::Rc;

struct Budget {
    used: Rc<Cell<usize>>,
    limit: Cell<usize>,
    reservations: Cell<usize>,
    fail_reservation: Cell<Option<usize>>,
    allocations: Cell<usize>,
    fail_allocation: Cell<Option<usize>>,
    live_pages: Rc<Cell<usize>>,
    largest: Cell<usize>,
}
struct Charge {
    used: Rc<Cell<usize>>,
    bytes: usize,
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.used.set(self.used.get() - self.bytes);
    }
}
impl StorageBudget for Budget {
    type Charge = Charge;
    type Error = ();
    fn reserve(&self, bytes: usize) -> Result<Charge, ()> {
        let call = self.reservations.get();
        self.reservations.set(call + 1);
        if self.fail_reservation.get() == Some(call) {
            return Err(());
        }
        let used = self.used.get().checked_add(bytes).ok_or(())?;
        if used > self.limit.get() {
            return Err(());
        }
        self.largest.set(self.largest.get().max(bytes));
        self.used.set(used);
        Ok(Charge {
            used: self.used.clone(),
            bytes,
        })
    }
}
struct TestPage {
    bytes: Box<[u8; PAGE_BYTES]>,
    live: Rc<Cell<usize>>,
    used: Rc<Cell<usize>>,
}
impl Drop for TestPage {
    fn drop(&mut self) {
        assert!(
            self.used.get() >= PAGE_BYTES,
            "page must remain charged until destruction"
        );
        self.live.set(self.live.get() - 1);
    }
}
impl Page for TestPage {
    fn bytes(&self) -> &[u8; PAGE_BYTES] {
        &self.bytes
    }
    fn bytes_mut(&mut self) -> &mut [u8; PAGE_BYTES] {
        &mut self.bytes
    }
}
impl Storage for Budget {
    type Page = TestPage;
    fn allocate_zeroed_page(&self) -> Result<TestPage, AllocationError> {
        let call = self.allocations.get();
        self.allocations.set(call + 1);
        if self.fail_allocation.get() == Some(call) {
            return Err(AllocationError);
        }
        let bytes = hyper::mm::try_box([0; PAGE_BYTES])?;
        self.live_pages.set(self.live_pages.get() + 1);
        Ok(TestPage {
            bytes,
            live: self.live_pages.clone(),
            used: self.used.clone(),
        })
    }
}
fn budget(limit: usize) -> Budget {
    Budget {
        used: Rc::new(Cell::new(0)),
        limit: Cell::new(limit),
        reservations: Cell::new(0),
        fail_reservation: Cell::new(None),
        allocations: Cell::new(0),
        fail_allocation: Cell::new(None),
        live_pages: Rc::new(Cell::new(0)),
        largest: Cell::new(0),
    }
}
fn contents(data: &FileData<'_, TestPage, Charge>) -> Vec<u8> {
    let mut bytes = vec![0xcc; data.len()];
    assert_eq!(data.read(0, &mut bytes), bytes.len());
    bytes
}

#[test]
fn sparse_write_and_truncate_never_resurrect_archive_or_old_bytes() {
    let budget = budget(1024 * 1024);
    let mut data = FileData::borrowed(b"archive");
    assert_eq!(data.resize(3, &budget), Ok(()));
    assert_eq!(data.archive(), Some(b"arc".as_slice()));
    assert_eq!(budget.used.get(), 0);
    assert_eq!(data.resize(6, &budget), Ok(()));
    assert_eq!(contents(&data), b"arc\0\0\0");
    assert_eq!(data.write(9, b"xy", &budget), Ok(2));
    assert_eq!(contents(&data), b"arc\0\0\0\0\0\0xy");
    assert_eq!(data.resize(2, &budget), Ok(()));
    assert_eq!(data.resize(11, &budget), Ok(()));
    assert_eq!(contents(&data), b"ar\0\0\0\0\0\0\0\0\0");
    assert_eq!(data.resize(0, &budget), Ok(()));
    assert_eq!(budget.used.get(), 0);
    assert_eq!(budget.live_pages.get(), 0);
    assert_eq!(data.write(1, b"z", &budget), Ok(1));
    assert_eq!(contents(&data), b"\0z");
    drop(data);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn rejected_growth_preserves_contents_and_exact_accounting() {
    let budget = budget(1024 * 1024);
    let mut data = FileData::borrowed(b"archive");
    assert_eq!(data.write(0, b"A", &budget), Ok(1));
    let used = budget.used.get();
    budget.limit.set(used);
    assert_eq!(
        data.write(PAGE_BYTES as u64, b"x", &budget),
        Err(Error::Budget(()))
    );
    assert_eq!(contents(&data), b"Archive");
    assert_eq!(budget.used.get(), used);
    assert_eq!(data.write(u64::MAX, b"x", &budget), Err(Error::Size));
    assert_eq!(contents(&data), b"Archive");
    // An overwrite needs no new page or index reservation at the quota limit.
    assert_eq!(data.write(1, b"R", &budget), Ok(1));
    assert_eq!(contents(&data), b"ARchive");
    drop(data);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn empty_write_and_past_end_read_do_not_allocate_or_extend() {
    let budget = budget(0);
    let mut data = FileData::borrowed(b"abc");
    assert_eq!(data.write(u64::MAX, b"", &budget), Ok(0));
    assert_eq!(contents(&data), b"abc");
    let mut output = [0xff; 4];
    assert_eq!(data.read(u64::MAX, &mut output), 0);
    assert_eq!(output, [0xff; 4]);
    assert_eq!(data.read(1, &mut output), 2);
    assert_eq!(output, [b'b', b'c', 0xff, 0xff]);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn every_failed_preparation_releases_its_prefix_without_publishing() {
    for owned in [false, true] {
        for allocation in [false, true] {
            let count = if allocation {
                5 - usize::from(owned)
            } else {
                7 - usize::from(owned)
            };
            for failure in 0..count {
                let budget = budget(1024 * 1024);
                let mut data = FileData::borrowed(b"archive");
                if owned {
                    assert_eq!(data.write(0, b"A", &budget), Ok(1));
                }
                let before = contents(&data);
                let borrowed = data.archive();
                let used = budget.used.get();
                let pages = budget.live_pages.get();
                if allocation {
                    budget
                        .fail_allocation
                        .set(Some(budget.allocations.get() + failure));
                } else {
                    budget
                        .fail_reservation
                        .set(Some(budget.reservations.get() + failure));
                }
                assert!(data.write(0, &vec![0xf1; 5 * PAGE_BYTES], &budget).is_err());
                assert_eq!(contents(&data), before);
                assert_eq!(data.archive(), borrowed);
                assert_eq!(budget.used.get(), used);
                assert_eq!(budget.live_pages.get(), pages);
                budget.fail_allocation.set(None);
                budget.fail_reservation.set(None);
                assert_eq!(data.resize((5 * PAGE_BYTES) as u64, &budget), Ok(()));
                assert_eq!(&contents(&data)[..before.len()], &before);
                assert!(contents(&data)[before.len()..].iter().all(|v| *v == 0));
                drop(data);
                assert_eq!(budget.used.get(), 0);
                assert_eq!(budget.live_pages.get(), 0);
            }
        }
    }
}

#[test]
fn truncate_releases_pages_and_zeroes_the_retained_tail() {
    let budget = budget(1024 * 1024);
    let mut data = FileData::borrowed(&[]);
    assert_eq!(
        data.write(0, &vec![0x7b; 5 * PAGE_BYTES], &budget),
        Ok(5 * PAGE_BYTES)
    );
    let used = budget.used.get();
    assert_eq!(data.resize((PAGE_BYTES + 7) as u64, &budget), Ok(()));
    assert_eq!(budget.live_pages.get(), 2);
    assert_eq!(budget.used.get(), used - 3 * PAGE_BYTES);
    assert_eq!(data.resize((4 * PAGE_BYTES) as u64, &budget), Ok(()));
    let bytes = contents(&data);
    assert!(bytes[..PAGE_BYTES + 7].iter().all(|v| *v == 0x7b));
    assert!(bytes[PAGE_BYTES + 7..].iter().all(|v| *v == 0));
}

#[test]
fn large_append_allocates_only_new_pages_and_small_index_storage() {
    const BLOCK: usize = 128 * 1024;
    const LENGTH: usize = 66 * 1024 * 1024 + 13;
    let budget = budget(80 * 1024 * 1024);
    let mut data = FileData::borrowed(&[]);
    let mut block = vec![0; BLOCK];
    for offset in (0..LENGTH).step_by(BLOCK) {
        let count = BLOCK.min(LENGTH - offset);
        block.fill((offset / BLOCK % 251) as u8);
        assert_eq!(
            data.write(offset as u64, &block[..count], &budget),
            Ok(count)
        );
    }
    assert_eq!(data.len(), LENGTH);
    assert_eq!(budget.allocations.get(), LENGTH.div_ceil(PAGE_BYTES));
    assert!(budget.largest.get() < 2 * 1024 * 1024);
    for offset in (0..LENGTH).step_by(BLOCK) {
        let count = BLOCK.min(LENGTH - offset);
        block.fill(0xff);
        assert_eq!(data.read(offset as u64, &mut block), count);
        assert!(
            block[..count]
                .iter()
                .all(|v| *v == (offset / BLOCK % 251) as u8)
        );
        assert!(block[count..].iter().all(|v| *v == 0xff));
    }
    drop(data);
    assert_eq!(budget.live_pages.get(), 0);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn page_boundaries_match_a_flat_reference_under_mixed_mutations() {
    let budget = budget(1024 * 1024);
    let archive = vec![0x8e; PAGE_BYTES + 9];
    let mut data = FileData::borrowed(&archive);
    let mut expected = archive.clone();
    let mut seed = 0x218d_1254_8309_aba1_u64;
    for step in 0..2000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let at = seed as usize % (8 * PAGE_BYTES);
        if step % 3 == 0 {
            assert_eq!(data.resize(at as u64, &budget), Ok(()));
            expected.resize(at, 0);
        } else {
            let input = vec![seed as u8; (seed >> 32) as usize % (2 * PAGE_BYTES)];
            assert_eq!(data.write(at as u64, &input, &budget), Ok(input.len()));
            if !input.is_empty() {
                expected.resize(expected.len().max(at + input.len()), 0);
                expected[at..at + input.len()].copy_from_slice(&input);
            }
        }
        assert_eq!(contents(&data), expected);
    }
    drop(data);
    assert_eq!(budget.used.get(), 0);
}
