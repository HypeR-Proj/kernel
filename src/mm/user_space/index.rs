// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Address-space metadata accounting for the shared persistent index.
use super::contract::{MemoryAccount, MemoryCharge};
pub(super) use hyper::collections::persistent_avl::{Entry, Error};
use hyper::collections::persistent_avl::{NodeAccount, PersistentAvl};

struct Accounting<A>(A);
impl<A: MemoryAccount> NodeAccount for Accounting<A> {
    type Charge = A::Charge;
    type Error = A::Error;
    fn try_charge(&self, bytes: usize) -> Result<Self::Charge, Self::Error> {
        self.0.try_charge(MemoryCharge {
            kernel_bytes: bytes as u64,
            ..MemoryCharge::default()
        })
    }
}

pub(super) struct Index<T: Entry<Key = u64, Weight = u64>, A: MemoryAccount>(
    PersistentAvl<T, Accounting<A>>,
);
impl<T: Entry<Key = u64, Weight = u64>, A: MemoryAccount> Clone for Index<T, A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Entry<Key = u64, Weight = u64>, A: MemoryAccount> Index<T, A> {
    pub(super) const fn new() -> Self {
        Self(PersistentAvl::new())
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(super) fn select(&self, index: usize) -> Option<&T> {
        self.0.select(index)
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }
    pub(super) fn iter_from(&self, lower: u64) -> impl Iterator<Item = &T> {
        self.0.iter_from(lower)
    }
    pub(super) fn get(&self, key: u64) -> Option<&T> {
        self.0.get(key)
    }
    pub(super) fn first(&self, key: u64, weight: u64) -> Option<&T> {
        self.0.first(key, weight)
    }
    pub(super) fn last(&self, key: u64, weight: u64) -> Option<&T> {
        self.0.last(key, weight)
    }
    pub(super) fn insert(&self, value: T, account: &A) -> Result<Self, Error<A::Error>> {
        self.0.insert(value, &Accounting(account.clone())).map(Self)
    }
    pub(super) fn remove(&self, key: u64, account: &A) -> Result<Self, Error<A::Error>> {
        self.0.remove(key, &Accounting(account.clone())).map(Self)
    }
}
