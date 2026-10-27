// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-VMAR free intervals. Child reservations and direct mappings remove
//! space from the same index; unmapping or destroying a child returns it.
//! Adjacent free intervals are always coalesced. Maximum-length augmentation
//! makes both nearest-hint queries and updates logarithmic in interval count.

use super::contract::{MemoryAccount, UserSlice};
use super::index::{Entry, Error, Index};

#[derive(Clone, Copy)]
struct Gap {
    base: u64,
    end: u64,
}
impl Entry for Gap {
    type Key = u64;
    type Weight = u64;
    fn key(&self) -> u64 {
        self.base
    }
    fn weight(&self) -> u64 {
        self.end - self.base
    }
}

pub(super) struct FreeRanges<A: MemoryAccount>(Index<Gap, A>);
impl<A: MemoryAccount> Clone for FreeRanges<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<A: MemoryAccount> FreeRanges<A> {
    pub(super) fn new(range: UserSlice, account: &A) -> Result<Self, Error<A::Error>> {
        Ok(Self(Index::new().insert(
            Gap {
                base: range.base().get(),
                end: range.end().get(),
            },
            account,
        )?))
    }
    pub(super) fn contains(&self, range: UserSlice) -> bool {
        self.0
            .last(range.base().get(), 0)
            .is_some_and(|gap| gap.end >= range.end().get())
    }
    pub(super) fn nearest(&self, size: u64, hint: u64) -> Option<u64> {
        let left = self.0.last(hint, size).map(|gap| hint.min(gap.end - size));
        let right = self.0.first(hint, size).map(|gap| gap.base);
        match (left, right) {
            (Some(l), Some(r)) => Some(if hint.abs_diff(l) <= hint.abs_diff(r) {
                l
            } else {
                r
            }),
            (Some(l), None) => Some(l),
            (None, right) => right,
        }
    }
    pub(super) fn reserve(
        &self,
        range: UserSlice,
        account: &A,
    ) -> Result<Option<Self>, Error<A::Error>> {
        let start = range.base().get();
        let end = range.end().get();
        let Some(gap) = self.0.last(start, 0).copied().filter(|gap| end <= gap.end) else {
            return Ok(None);
        };
        let mut replacement = self.0.remove(gap.base, account)?;
        if gap.base < start {
            replacement = replacement.insert(
                Gap {
                    base: gap.base,
                    end: start,
                },
                account,
            )?;
        }
        if end < gap.end {
            replacement = replacement.insert(
                Gap {
                    base: end,
                    end: gap.end,
                },
                account,
            )?;
        }
        Ok(Some(Self(replacement)))
    }
    pub(super) fn release(
        &self,
        range: UserSlice,
        account: &A,
    ) -> Result<Option<Self>, Error<A::Error>> {
        let mut base = range.base().get();
        let mut end = range.end().get();
        let left = self.0.last(base, 0).copied();
        let right = self.0.first(base, 0).copied();
        if left.is_some_and(|gap| gap.end > base) || right.is_some_and(|gap| gap.base < end) {
            return Ok(None);
        }
        let mut replacement = self.0.clone();
        if let Some(gap) = left.filter(|gap| gap.end == base) {
            replacement = replacement.remove(gap.base, account)?;
            base = gap.base;
        }
        if let Some(gap) = right.filter(|gap| gap.base == end) {
            replacement = replacement.remove(gap.base, account)?;
            end = gap.end;
        }
        Ok(Some(Self(replacement.insert(Gap { base, end }, account)?)))
    }
}
