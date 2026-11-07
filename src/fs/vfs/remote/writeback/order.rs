// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Logical receipt order stays independent of reusable physical batch slots.

pub(super) struct Order<const N: usize> {
    head: usize,
    length: usize,
}

impl<const N: usize> Order<N> {
    pub(super) const fn new() -> Self {
        Self { head: 0, length: 0 }
    }
    pub(super) fn oldest(&self) -> Option<usize> {
        (self.length != 0).then_some(self.head)
    }
    pub(super) fn vacant(&self) -> Option<usize> {
        (self.length < N).then(|| (self.head + self.length) % N)
    }
    pub(super) fn admit(&mut self, slot: usize) -> bool {
        if self.vacant() != Some(slot) {
            return false;
        }
        self.length += 1;
        true
    }
    pub(super) fn retire(&mut self, slot: usize) -> bool {
        if self.oldest() != Some(slot) {
            return false;
        }
        self.head = (self.head + 1) % N;
        self.length -= 1;
        true
    }
    pub(super) fn active(&self) -> impl Iterator<Item = usize> + use<N> {
        let head = self.head;
        (0..self.length).map(move |offset| (head + offset) % N)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refill_reuses_retired_slot_without_overtaking_older_receipt() {
        let mut order = Order::<2>::new();
        assert_eq!(order.vacant(), Some(0));
        assert!(order.admit(0));
        assert!(order.admit(1));
        assert_eq!(order.vacant(), None);
        assert!(!order.admit(0));
        assert!(!order.retire(1));
        assert!(order.retire(0));
        assert!(order.admit(0));
        assert_eq!(order.active().collect::<alloc::vec::Vec<_>>(), [1, 0]);
        assert!(!order.retire(0));
        assert!(order.retire(1));
        assert!(order.retire(0));
        assert_eq!(order.oldest(), None);
        assert!(!order.retire(1));
        for _ in 0..20 {
            let Some(slot) = order.vacant() else {
                panic!("empty ring has no slot")
            };
            assert!(order.admit(slot));
            assert_eq!(order.oldest(), Some(slot));
            assert!(order.retire(slot));
        }
    }

    #[test]
    fn empty_capacity_never_performs_modulo_zero() {
        let order = Order::<0>::new();
        assert_eq!(order.vacant(), None);
        assert_eq!(order.oldest(), None);
        assert_eq!(order.active().count(), 0);
    }
}
