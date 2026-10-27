// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Address-ordered mapping snapshots. Only search paths change during updates;
//! shared nodes retain backing and quota until the last snapshot retires.

use super::*;

pub(super) type MappingIndex<B, A> = Index<Mapping<B, A>, A>;

impl<B: PageBackend, A: MemoryAccount> Entry for Mapping<B, A> {
    type Key = u64;
    type Weight = u64;
    fn key(&self) -> u64 {
        self.snapshot.range.base().get()
    }
}

impl<B: PageBackend, A: MemoryAccount> MappingIndex<B, A> {
    pub(super) fn containing(&self, address: UserAddress) -> Option<&Mapping<B, A>> {
        self.last(address.get(), 0)
            .filter(|mapping| address < mapping.snapshot.range.end())
    }

    /// Mappings are disjoint. The predecessor can be the only overlapping
    /// entry whose base precedes the query; later entries are visited in order.
    pub(super) fn overlapping(&self, range: UserSlice) -> impl Iterator<Item = &Mapping<B, A>> {
        self.iter_from_address(range.base().get())
            .take_while(move |mapping| {
                range.length() != 0 && mapping.snapshot.range.base() < range.end()
            })
    }

    pub(super) fn iter_from_address(&self, address: u64) -> impl Iterator<Item = &Mapping<B, A>> {
        let lower = self
            .last(address, 0)
            .map_or(address, |mapping| mapping.key());
        self.iter_from(lower)
            .skip_while(move |mapping| mapping.snapshot.range.end().get() <= address)
    }

    pub(super) fn by_token(&self, token: MappingToken) -> Option<&Mapping<B, A>> {
        self.get(token.base.get())
            .filter(|mapping| mapping.snapshot.token == token)
    }

    /// Protection/unmap fragments of one private view must all move to the
    /// same version. This is an alias walk, not an address lookup; updating the
    /// tree while iterating a retained root cannot skip or revisit fragments.
    pub(super) fn replace_private_view(
        &mut self,
        old: &FallibleArc<PrivateView<B, A>>,
        next: &FallibleArc<PrivateView<B, A>>,
        account: &A,
    ) -> SpaceResult<B, A, ()> {
        let previous = self.clone();
        for mapping in previous.iter() {
            if mapping
                .object
                .private()
                .is_some_and(|view| core::ptr::eq(&**view, &**old))
            {
                let mut changed = mapping.clone();
                changed.object = MappingObject::Private(next.clone());
                *self = self.insert(changed, account)?;
            }
        }
        Ok(())
    }
}
