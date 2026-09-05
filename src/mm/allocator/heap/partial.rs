// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Typed, constant-time topology for intrusive partial-slab lists.

const NONE: u64 = u64::MAX;
pub(super) const SLAB_PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SlabClass<const CLASSES: usize>(u8);

impl<const CLASSES: usize> SlabClass<CLASSES> {
    pub(super) const fn new(index: usize) -> Option<Self> {
        if index < CLASSES && index <= u8::MAX as usize {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    pub(super) const fn index(self) -> usize {
        self.0 as usize
    }

    pub(super) const fn raw(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SlabPageId(u64);

impl SlabPageId {
    pub(super) const fn new(physical: u64) -> Option<Self> {
        if physical == NONE || physical & (SLAB_PAGE_SIZE - 1) != 0 {
            None
        } else {
            Some(Self(physical))
        }
    }

    pub(super) const fn physical(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SlabLink(u64);

impl SlabLink {
    pub(super) const NONE: Self = Self(NONE);

    pub(super) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(super) const fn from_page(page: SlabPageId) -> Self {
        Self(page.0)
    }

    pub(super) const fn decode(self) -> Result<Option<SlabPageId>, InvalidTopology> {
        if self.0 == NONE {
            Ok(None)
        } else {
            match SlabPageId::new(self.0) {
                Some(page) => Ok(Some(page)),
                None => Err(InvalidTopology),
            }
        }
    }

    pub(super) const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PartialLinks {
    pub(super) previous: SlabLink,
    pub(super) next: SlabLink,
}

impl PartialLinks {
    pub(super) const DETACHED: Self = Self {
        previous: SlabLink::NONE,
        next: SlabLink::NONE,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PartialNode<const CLASSES: usize> {
    pub(super) class: SlabClass<CLASSES>,
    pub(super) links: PartialLinks,
    pub(super) linked: bool,
}

pub(super) trait PartialNodeStore<const CLASSES: usize> {
    type Error;

    fn resolve(&self, page: SlabPageId) -> Result<PartialNode<CLASSES>, Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidTopology;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PreflightError<E> {
    Resolve(E),
    InvalidTopology,
}

pub(super) struct HeadPermit<'a, const CLASSES: usize> {
    lists: &'a mut PartialSlabLists<CLASSES>,
    class: SlabClass<CLASSES>,
    head: Option<SlabPageId>,
}

impl<'a, const CLASSES: usize> HeadPermit<'a, CLASSES> {
    pub(super) const fn head(&self) -> Option<SlabPageId> {
        self.head
    }

    /// Consumes the exact head observed before a newly allocated page exists.
    pub(super) fn prepare_new_page(self, target: SlabPageId) -> Option<InsertPermit<'a, CLASSES>> {
        if self.head == Some(target) {
            None
        } else {
            Some(InsertPermit {
                lists: self.lists,
                class: self.class,
                target,
                old_head: self.head,
            })
        }
    }
}

pub(super) struct InsertPermit<'a, const CLASSES: usize> {
    lists: &'a mut PartialSlabLists<CLASSES>,
    class: SlabClass<CLASSES>,
    target: SlabPageId,
    old_head: Option<SlabPageId>,
}

impl<const CLASSES: usize> InsertPermit<'_, CLASSES> {
    pub(super) const fn target(&self) -> SlabPageId {
        self.target
    }

    pub(super) const fn old_head(&self) -> Option<SlabPageId> {
        self.old_head
    }

    pub(super) fn commit(self) {
        self.lists.heads[self.class.index()] = Some(self.target);
    }
}

pub(super) struct RemovePermit<'a, const CLASSES: usize> {
    lists: &'a mut PartialSlabLists<CLASSES>,
    class: SlabClass<CLASSES>,
    target: SlabPageId,
    previous: Option<SlabPageId>,
    next: Option<SlabPageId>,
}

impl<const CLASSES: usize> RemovePermit<'_, CLASSES> {
    pub(super) const fn target(&self) -> SlabPageId {
        self.target
    }

    pub(super) const fn previous(&self) -> Option<SlabPageId> {
        self.previous
    }

    pub(super) const fn next(&self) -> Option<SlabPageId> {
        self.next
    }

    pub(super) fn commit(self) {
        if self.previous.is_none() {
            self.lists.heads[self.class.index()] = self.next;
        }
    }
}

pub(super) struct PartialSlabLists<const CLASSES: usize> {
    heads: [Option<SlabPageId>; CLASSES],
}

impl<const CLASSES: usize> PartialSlabLists<CLASSES> {
    pub(super) const fn new() -> Self {
        Self {
            heads: [None; CLASSES],
        }
    }

    pub(super) const fn class(&self, index: usize) -> Option<SlabClass<CLASSES>> {
        SlabClass::new(index)
    }

    pub(super) const fn head(&self, class: SlabClass<CLASSES>) -> Option<SlabPageId> {
        self.heads[class.index()]
    }

    pub(super) fn preflight_insert<'a, S: PartialNodeStore<CLASSES>>(
        &'a mut self,
        store: &S,
        class: SlabClass<CLASSES>,
        target: SlabPageId,
    ) -> Result<InsertPermit<'a, CLASSES>, PreflightError<S::Error>> {
        let target_node = store.resolve(target).map_err(PreflightError::Resolve)?;
        if target_node.class != class
            || target_node.linked
            || target_node.links != PartialLinks::DETACHED
        {
            return Err(PreflightError::InvalidTopology);
        }
        self.preflight_head(store, class)?
            .prepare_new_page(target)
            .ok_or(PreflightError::InvalidTopology)
    }

    pub(super) fn preflight_head<'a, S: PartialNodeStore<CLASSES>>(
        &'a mut self,
        store: &S,
        class: SlabClass<CLASSES>,
    ) -> Result<HeadPermit<'a, CLASSES>, PreflightError<S::Error>> {
        let head = self.head(class);
        if let Some(head) = head {
            let head_node = store.resolve(head).map_err(PreflightError::Resolve)?;
            if head_node.class != class
                || !head_node.linked
                || head_node.links.previous != SlabLink::NONE
            {
                return Err(PreflightError::InvalidTopology);
            }
        }
        Ok(HeadPermit {
            lists: self,
            class,
            head,
        })
    }

    pub(super) fn preflight_remove<'a, S: PartialNodeStore<CLASSES>>(
        &'a mut self,
        store: &S,
        class: SlabClass<CLASSES>,
        target: SlabPageId,
    ) -> Result<RemovePermit<'a, CLASSES>, PreflightError<S::Error>> {
        let target_node = store.resolve(target).map_err(PreflightError::Resolve)?;
        if target_node.class != class || !target_node.linked {
            return Err(PreflightError::InvalidTopology);
        }
        let previous = target_node
            .links
            .previous
            .decode()
            .map_err(|_| PreflightError::InvalidTopology)?;
        let next = target_node
            .links
            .next
            .decode()
            .map_err(|_| PreflightError::InvalidTopology)?;
        if previous == Some(target)
            || next == Some(target)
            || (previous.is_some() && previous == next)
        {
            return Err(PreflightError::InvalidTopology);
        }

        let Some(head) = self.head(class) else {
            return Err(PreflightError::InvalidTopology);
        };
        let head_node = if head == target {
            target_node
        } else {
            store.resolve(head).map_err(PreflightError::Resolve)?
        };
        if head_node.class != class
            || !head_node.linked
            || head_node.links.previous != SlabLink::NONE
            || (previous.is_none() && head != target)
            || (previous.is_some() && head == target)
        {
            return Err(PreflightError::InvalidTopology);
        }

        if let Some(previous) = previous {
            let previous_node = if previous == head {
                head_node
            } else {
                store.resolve(previous).map_err(PreflightError::Resolve)?
            };
            if previous_node.class != class
                || !previous_node.linked
                || previous_node.links.next != SlabLink::from_page(target)
            {
                return Err(PreflightError::InvalidTopology);
            }
        }
        if let Some(next) = next {
            let next_node = store.resolve(next).map_err(PreflightError::Resolve)?;
            if next_node.class != class
                || !next_node.linked
                || next_node.links.previous != SlabLink::from_page(target)
            {
                return Err(PreflightError::InvalidTopology);
            }
        }
        Ok(RemovePermit {
            lists: self,
            class,
            target,
            previous,
            next,
        })
    }
}
