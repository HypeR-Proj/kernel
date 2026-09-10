// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reservation lifetime and failure atomicity of filesystem request buffers.

use std::cell::RefCell;
use std::rc::Rc;

use hyper::fs::file_data::StorageBudget;
use hyper::fs::scratch::{BudgetedString, BudgetedVec, Error};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Quota;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Reserved(usize),
    Released(usize),
    ElementDropped(u8),
}

struct Ledger {
    used: u128,
    peak: u128,
    limit: u128,
    events: Vec<Event>,
}

#[derive(Clone)]
struct Budget(Rc<RefCell<Ledger>>);

struct Charge {
    ledger: Rc<RefCell<Ledger>>,
    bytes: usize,
}

impl Budget {
    fn new(limit: u128) -> Self {
        Self(Rc::new(RefCell::new(Ledger {
            used: 0,
            peak: 0,
            limit,
            events: Vec::new(),
        })))
    }
    fn used(&self) -> u128 {
        self.0.borrow().used
    }
}

impl StorageBudget for Budget {
    type Charge = Charge;
    type Error = Quota;

    fn reserve(&self, bytes: usize) -> Result<Charge, Quota> {
        let mut ledger = self.0.borrow_mut();
        let next = ledger.used + bytes as u128;
        if next > ledger.limit {
            return Err(Quota);
        }
        ledger.used = next;
        ledger.peak = ledger.peak.max(next);
        ledger.events.push(Event::Reserved(bytes));
        Ok(Charge {
            ledger: self.0.clone(),
            bytes,
        })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let mut ledger = self.ledger.borrow_mut();
        assert!(ledger.used >= self.bytes as u128);
        ledger.used -= self.bytes as u128;
        ledger.events.push(Event::Released(self.bytes));
    }
}

#[test]
fn growth_reserves_old_and_new_capacity_before_releasing_old_charge() {
    let budget = Budget::new(64);
    let mut values = BudgetedVec::<u64, _>::new(budget.clone());
    crate::require_ok(values.try_reserve_exact(2));
    crate::require_ok(values.push(11));
    crate::require_ok(values.push(22));
    assert_eq!(budget.used(), 16);
    crate::require_ok(values.push(33));
    assert_eq!(&*values, &[11, 22, 33]);
    assert_eq!(values.capacity(), 4);
    assert_eq!(budget.used(), 32);
    assert_eq!(budget.0.borrow().peak, 48);
    assert_eq!(
        budget.0.borrow().events,
        [
            Event::Reserved(16),
            Event::Reserved(32),
            Event::Released(16)
        ]
    );
    drop(values);
    assert_eq!(budget.used(), 0);
}

#[test]
fn replacement_quota_failure_preserves_data_capacity_and_reservation() {
    let budget = Budget::new(8);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.extend_from_slice(&[1_u8, 2, 3, 4]));
    assert_eq!(values.push(5), Err(Error::Budget(Quota)));
    assert_eq!(&*values, &[1, 2, 3, 4]);
    assert_eq!(values.capacity(), 4);
    assert_eq!(budget.used(), 4);
    assert_eq!(budget.0.borrow().events, [Event::Reserved(4)]);
    drop(values);
    assert_eq!(budget.used(), 0);
}

#[test]
fn allocation_failure_releases_new_reservation_without_touching_old_storage() {
    // Vec rejects a byte layout larger than isize::MAX without asking the
    // system allocator for enormous memory. The budget deliberately accepts it
    // so this exercises the allocation failure path after admission.
    let budget = Budget::new(u128::MAX);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.push(7_u8));
    assert_eq!(
        values.try_reserve_exact(usize::MAX - 1),
        Err(Error::Allocation)
    );
    assert_eq!(&*values, &[7]);
    assert_eq!(values.capacity(), 1);
    assert_eq!(budget.used(), 1);
    assert_eq!(
        budget.0.borrow().events,
        [
            Event::Reserved(1),
            Event::Reserved(usize::MAX),
            Event::Released(usize::MAX)
        ]
    );
    drop(values);
    assert_eq!(budget.used(), 0);
}

#[test]
fn arithmetic_failure_precedes_admission() {
    let budget = Budget::new(u128::MAX);
    let mut values = BudgetedVec::<u64, _>::new(budget.clone());
    assert_eq!(values.try_reserve_exact(usize::MAX), Err(Error::Size));
    assert!(budget.0.borrow().events.is_empty());
    crate::require_ok(values.push(1));
    let events = budget.0.borrow().events.clone();
    assert_eq!(values.try_reserve(usize::MAX), Err(Error::Size));
    assert_eq!(budget.0.borrow().events, events);
    assert_eq!(&*values, &[1]);
}

#[test]
fn draining_and_iterator_exhaustion_retain_allocated_capacity_charge() {
    let budget = Budget::new(64);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.extend_from_slice(&[1_u32, 2, 3]));
    assert_eq!(values.pop(), Some(3));
    values.clear();
    assert_eq!(budget.used(), 16);
    crate::require_ok(values.extend_from_slice(&[4, 5, 6]));
    let mut iter = values.into_iter();
    assert_eq!(iter.len(), 3);
    assert_eq!(iter.next(), Some(4));
    assert_eq!(iter.next_back(), Some(6));
    assert_eq!(iter.next(), Some(5));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.len(), 0);
    assert_eq!(budget.used(), 16);
    drop(iter);
    assert_eq!(budget.used(), 0);
}

struct ObservedDrop {
    ledger: Rc<RefCell<Ledger>>,
    id: u8,
}
impl Drop for ObservedDrop {
    fn drop(&mut self) {
        let mut ledger = self.ledger.borrow_mut();
        assert!(ledger.used > 0);
        ledger.events.push(Event::ElementDropped(self.id));
    }
}

#[test]
fn iterator_destroys_remaining_elements_before_releasing_storage_charge() {
    let budget = Budget::new(1024);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.try_reserve_exact(3));
    for id in 0..3 {
        crate::require_ok(values.push(ObservedDrop {
            ledger: budget.0.clone(),
            id,
        }));
    }
    let bytes = values.capacity() * core::mem::size_of::<ObservedDrop>();
    let mut iter = values.into_iter();
    drop(iter.next());
    assert_eq!(budget.used(), bytes as u128);
    drop(iter);
    assert_eq!(
        budget.0.borrow().events,
        [
            Event::Reserved(bytes),
            Event::ElementDropped(0),
            Event::ElementDropped(1),
            Event::ElementDropped(2),
            Event::Released(bytes)
        ]
    );
}

#[test]
fn utf8_growth_and_quota_failure_preserve_complete_characters() {
    let budget = Budget::new(16);
    let mut text = crate::require_ok(BudgetedString::from_str("é", budget.clone()));
    crate::require_ok(text.push('界'));
    assert_eq!(text.as_str(), "é界");
    assert_eq!(budget.used(), 8);
    assert_eq!(text.push('🦀'), Err(Error::Budget(Quota)));
    assert_eq!(&*text, "é界");
    assert_eq!(text.capacity(), 8);
    assert_eq!(budget.used(), 8);
    assert_eq!(format!("{text:?}"), "\"é界\"");
    drop(text);
    assert_eq!(budget.used(), 0);
}

#[test]
fn zero_sized_elements_need_no_physical_capacity_reservation() {
    let budget = Budget::new(0);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.resize(8, ()));
    assert_eq!(values.len(), 8);
    assert!(budget.0.borrow().events.is_empty());
    let mut iter = values.into_iter();
    assert_eq!(iter.next(), Some(()));
    drop(iter);
    assert_eq!(budget.used(), 0);
}

#[test]
fn direct_drop_destroys_elements_before_releasing_capacity() {
    let budget = Budget::new(1024);
    let mut values = BudgetedVec::new(budget.clone());
    crate::require_ok(values.try_reserve_exact(2));
    for id in 0..2 {
        crate::require_ok(values.push(ObservedDrop {
            ledger: budget.0.clone(),
            id,
        }));
    }
    let bytes = values.capacity() * core::mem::size_of::<ObservedDrop>();
    drop(values);
    assert_eq!(
        budget.0.borrow().events,
        [
            Event::Reserved(bytes),
            Event::ElementDropped(0),
            Event::ElementDropped(1),
            Event::Released(bytes)
        ]
    );
    assert_eq!(budget.used(), 0);
}

#[test]
fn utf8_conversion_moves_existing_storage_or_releases_rejected_bytes() {
    let budget = Budget::new(32);
    let mut bytes = BudgetedVec::new(budget.clone());
    crate::require_ok(bytes.extend_from_slice("界".as_bytes()));
    let pointer = bytes.as_ptr();
    let charged = budget.used();
    let text = crate::require_ok(BudgetedString::from_utf8(bytes));
    assert_eq!(text.as_str(), "界");
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(budget.used(), charged);
    drop(text);
    assert_eq!(budget.used(), 0);

    let mut invalid = BudgetedVec::new(budget.clone());
    crate::require_ok(invalid.extend_from_slice(&[0xf0, 0x9f]));
    assert!(BudgetedString::from_utf8(invalid).is_err());
    assert_eq!(budget.used(), 0);
}
