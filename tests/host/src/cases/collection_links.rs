// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::collections::linked_list::{self, NextLink, TailLink};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

struct Node {
    value: u32,
    next: Option<Box<Node>>,
    drops: Rc<Cell<usize>>,
}
impl Drop for Node {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
impl NextLink for Box<Node> {
    fn next(&self) -> &Option<Self> {
        &self.next
    }
    fn next_mut(&mut self) -> &mut Option<Self> {
        &mut self.next
    }
}
#[test]
fn singly_linked_operations_preserve_order_and_return_detached_owners() {
    let drops = Rc::new(Cell::new(0));
    let mut head = None;
    for value in [3, 1, 4, 2] {
        let node = Box::new(Node {
            value,
            next: None,
            drops: drops.clone(),
        });
        assert!(linked_list::insert_before(&mut head, node, |a, b| a.value < b.value).is_ok());
    }
    assert!(linked_list::remove_first(&mut head, |n| n.value == 9).is_none());
    let removed = crate::require_some(linked_list::remove_first(&mut head, |n| n.value == 3));
    assert!(removed.next.is_none());
    assert_eq!(drops.get(), 0);
    drop(removed);
    for expected in [1, 2, 4] {
        let node = crate::require_some(linked_list::pop_front(&mut head));
        assert_eq!(node.value, expected);
        assert!(node.next.is_none());
    }
    assert!(head.is_none());
    assert_eq!(drops.get(), 4);
}
#[test]
fn linked_node_rejection_preserves_input_and_destination() {
    let drops = Rc::new(Cell::new(0));
    let make = |value| {
        Box::new(Node {
            value,
            next: None,
            drops: drops.clone(),
        })
    };
    let mut head = Some(make(7));
    let mut node = make(1);
    node.next = Some(make(2));
    let rejected = match linked_list::insert_before(&mut head, node, |_, _| true) {
        Err(node) => node,
        Ok(()) => panic!("linked input accepted"),
    };
    assert_eq!(head.as_ref().map(|n| n.value), Some(7));
    assert_eq!(rejected.next.as_ref().map(|n| n.value), Some(2));
    assert_eq!(drops.get(), 0);
}
#[derive(Clone)]
struct Handle(Rc<Item>);
struct Item {
    id: u32,
    next: RefCell<Option<Handle>>,
    drops: Rc<Cell<usize>>,
}
impl Drop for Item {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
impl TailLink for Handle {
    fn link_successor(&self, next: Self) {
        let mut link = self.0.next.borrow_mut();
        assert!(link.is_none());
        *link = Some(next);
    }
    fn take_successor(&self) -> Option<Self> {
        self.0.next.borrow_mut().take()
    }
}
#[test]
fn retirement_splice_preserves_fifo_and_releases_only_caller_dropped_nodes() {
    let drops = Rc::new(Cell::new(0));
    let mut head = None;
    let mut tail = None;
    let mut other = None;
    let mut other_tail = None;
    for id in 0..4 {
        let node = Handle(Rc::new(Item {
            id,
            next: RefCell::new(None),
            drops: drops.clone(),
        }));
        if id < 2 {
            linked_list::push_back(&mut head, &mut tail, node);
        } else {
            linked_list::push_back(&mut other, &mut other_tail, node);
        }
    }
    linked_list::append(&mut head, &mut tail, &mut other, &mut other_tail);
    assert!(other.is_none() && other_tail.is_none());
    linked_list::append(&mut head, &mut tail, &mut other, &mut other_tail);
    for id in 0..4 {
        let node = crate::require_some(linked_list::pop_front_with_tail(&mut head, &mut tail));
        assert_eq!(node.0.id, id);
        assert!(node.0.next.borrow().is_none());
        assert_eq!(Rc::strong_count(&node.0), 1);
        assert_eq!(drops.get(), id as usize);
        drop(node);
    }
    assert!(head.is_none() && tail.is_none());
    assert_eq!(drops.get(), 4);
    assert!(linked_list::pop_front_with_tail(&mut head, &mut tail).is_none());
}

#[test]
fn bitmap_word_boundaries_and_empty_ranges_are_checked() {
    use hyper::collections::fixed_bitmap::{Error, FixedBitmap, storage_bytes};
    let word = usize::BITS as usize;
    for count in [0, 1, word - 1, word, word + 1, 3 * word + 7] {
        let mut bits = crate::require_ok(FixedBitmap::try_new(count));
        assert_eq!(bits.len(), count);
        assert_eq!(bits.is_empty(), count == 0);
        assert_eq!(Some(bits.retained_bytes()), storage_bytes(count));
        assert!(bits.iter().all(|b| !b));
        for i in 0..count {
            crate::require_ok(bits.set(i, i % 3 == 0));
        }
        for i in 0..count {
            assert_eq!(bits.get(i), Some(i % 3 == 0));
        }
        assert_eq!(bits.get(count), None);
        assert_eq!(bits.set(count, true), Err(Error::InvalidRange));
        for i in 0..count {
            crate::require_ok(bits.set(i, false));
        }
        assert!(bits.iter().all(|b| !b));
    }
    assert_eq!(storage_bytes(usize::MAX), None);
    assert!(matches!(
        FixedBitmap::try_new(usize::MAX),
        Err(Error::Allocation)
    ));
}
