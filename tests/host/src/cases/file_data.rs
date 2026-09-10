// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::fs::file_data::{Error, FileData, StorageBudget};
use std::cell::Cell;
use std::rc::Rc;

struct Budget {
    used: Rc<Cell<usize>>,
    limit: usize,
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
        let used = self.used.get().checked_add(bytes).ok_or(())?;
        if used > self.limit {
            return Err(());
        }
        self.used.set(used);
        Ok(Charge {
            used: self.used.clone(),
            bytes,
        })
    }
}
fn budget(limit: usize) -> Budget {
    Budget {
        used: Rc::new(Cell::new(0)),
        limit,
    }
}

#[test]
fn sparse_write_and_truncate_never_resurrect_archive_or_old_bytes() {
    let budget = budget(1024);
    let mut data = FileData::borrowed(b"archive");
    assert_eq!(data.resize(3, &budget), Ok(()));
    assert_eq!(data.resize(6, &budget), Ok(()));
    assert_eq!(data.bytes(), b"arc\0\0\0");
    assert_eq!(data.write(9, b"xy", &budget), Ok(2));
    assert_eq!(data.bytes(), b"arc\0\0\0\0\0\0xy");
    assert_eq!(data.resize(2, &budget), Ok(()));
    assert_eq!(data.resize(11, &budget), Ok(()));
    assert_eq!(data.bytes(), b"ar\0\0\0\0\0\0\0\0\0");
    assert_eq!(data.resize(0, &budget), Ok(()));
    assert_eq!(budget.used.get(), 0);
    assert_eq!(data.write(1, b"z", &budget), Ok(1));
    assert_eq!(data.bytes(), b"\0z");
    drop(data);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn rejected_growth_preserves_contents_and_exact_accounting() {
    let budget = budget(16);
    let mut data = FileData::borrowed(b"archive");
    assert_eq!(data.write(0, b"A", &budget), Ok(1));
    assert_eq!(budget.used.get(), 8);
    assert_eq!(data.write(8, b"x", &budget), Err(Error::Budget(())));
    assert_eq!(data.bytes(), b"Archive");
    assert_eq!(budget.used.get(), 8);
    assert_eq!(data.write(u64::MAX, b"x", &budget), Err(Error::Size));
    assert_eq!(data.bytes(), b"Archive");
    drop(data);
    assert_eq!(budget.used.get(), 0);
}

#[test]
fn empty_write_and_past_end_read_do_not_allocate_or_extend() {
    let budget = budget(0);
    let mut data = FileData::borrowed(b"abc");
    assert_eq!(data.write(u64::MAX, b"", &budget), Ok(0));
    assert_eq!(data.bytes(), b"abc");
    let mut output = [0xff; 4];
    assert_eq!(data.read(u64::MAX, &mut output), 0);
    assert_eq!(output, [0xff; 4]);
    assert_eq!(data.read(1, &mut output), 2);
    assert_eq!(output, [b'b', b'c', 0xff, 0xff]);
}
