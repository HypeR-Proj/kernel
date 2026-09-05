// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/mm/allocator/heap/partial.rs"]
mod model;

use std::cell::Cell;

use model::{
    InvalidTopology, PartialLinks, PartialNode, PartialNodeStore, PartialSlabLists, PreflightError,
    SlabClass, SlabLink, SlabPageId,
};

const CLASSES: usize = 2;
type TestClass = SlabClass<CLASSES>;
type TestNode = PartialNode<CLASSES>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    page: SlabPageId,
    node: TestNode,
}

struct FakeStore {
    entries: Vec<Entry>,
    resolutions: Cell<usize>,
}

impl FakeStore {
    fn list(length: usize, class: TestClass) -> Self {
        let mut entries = Vec::new();
        for index in 0..length {
            let current_page = page(index + 1);
            let previous = if index == 0 {
                SlabLink::NONE
            } else {
                SlabLink::from_page(page(index))
            };
            let next = if index + 1 == length {
                SlabLink::NONE
            } else {
                SlabLink::from_page(page(index + 2))
            };
            entries.push(Entry {
                page: current_page,
                node: PartialNode {
                    class,
                    links: PartialLinks { previous, next },
                    linked: true,
                },
            });
        }
        Self {
            entries,
            resolutions: Cell::new(0),
        }
    }

    fn reset_resolutions(&self) {
        self.resolutions.set(0);
    }
}

impl PartialNodeStore<CLASSES> for FakeStore {
    type Error = ();

    fn resolve(&self, page: SlabPageId) -> Result<TestNode, Self::Error> {
        self.resolutions.set(self.resolutions.get() + 1);
        self.entries
            .iter()
            .find(|entry| entry.page == page)
            .map(|entry| entry.node)
            .ok_or(())
    }
}

fn page(index: usize) -> SlabPageId {
    crate::require_some(SlabPageId::new(index as u64 * hyper::mm::PAGE_SIZE))
}

fn lists_with_head(
    store: &FakeStore,
    class: TestClass,
    head: SlabPageId,
) -> PartialSlabLists<CLASSES> {
    let mut lists = PartialSlabLists::new();
    let permit = crate::require_ok(lists.preflight_head(store, class));
    let permit = crate::require_some(permit.prepare_new_page(head));
    permit.commit();
    lists
}

#[test]
fn preflights_singleton_head_middle_and_tail_in_constant_space() {
    let class = crate::require_some(SlabClass::new(0));
    for (length, target, expected_previous, expected_next, maximum_resolutions) in [
        (1, 1, None, None, 1),
        (4, 1, None, Some(2), 2),
        (4, 2, Some(1), Some(3), 3),
        (4, 4, Some(3), None, 3),
    ] {
        let store = FakeStore::list(length, class);
        let mut lists = lists_with_head(&store, class, page(1));
        let permit = crate::require_ok(lists.preflight_remove(&store, class, page(target)));
        assert_eq!(permit.previous(), expected_previous.map(page));
        assert_eq!(permit.next(), expected_next.map(page));
        assert!(store.resolutions.get() <= maximum_resolutions);
    }
}

#[test]
fn corrupt_reciprocal_link_fails_without_mutating_store_or_head() {
    let class = crate::require_some(SlabClass::new(0));
    let mut store = FakeStore::list(3, class);
    store.entries[2].node.links.previous = SlabLink::from_page(page(1));
    let before = store.entries.clone();
    let mut lists = lists_with_head(&store, class, page(1));

    assert!(matches!(
        lists.preflight_remove(&store, class, page(2)),
        Err(PreflightError::InvalidTopology)
    ));
    assert_eq!(store.entries, before);
    assert_eq!(lists.head(class), Some(page(1)));
}

#[test]
fn rejects_self_links_wrong_classes_and_missing_neighbors_without_commit() {
    let class = crate::require_some(SlabClass::new(0));
    let other_class = crate::require_some(SlabClass::new(1));

    let mut self_link = FakeStore::list(3, class);
    let mut lists = lists_with_head(&self_link, class, page(1));
    self_link.entries[1].node.links.next = SlabLink::from_page(page(2));
    let before = self_link.entries.clone();
    assert!(matches!(
        lists.preflight_remove(&self_link, class, page(2)),
        Err(PreflightError::InvalidTopology)
    ));
    assert_eq!(self_link.entries, before);

    let mut wrong_class = FakeStore::list(3, class);
    wrong_class.entries[1].node.class = other_class;
    let before = wrong_class.entries.clone();
    assert!(matches!(
        lists.preflight_remove(&wrong_class, class, page(2)),
        Err(PreflightError::InvalidTopology)
    ));
    assert_eq!(wrong_class.entries, before);

    let mut unaligned_link = FakeStore::list(3, class);
    unaligned_link.entries[1].node.links.previous = SlabLink::from_raw(hyper::mm::PAGE_SIZE + 1);
    let before = unaligned_link.entries.clone();
    assert!(matches!(
        lists.preflight_remove(&unaligned_link, class, page(2)),
        Err(PreflightError::InvalidTopology)
    ));
    assert_eq!(unaligned_link.entries, before);

    let mut missing_neighbor = FakeStore::list(3, class);
    let _ = missing_neighbor.entries.pop();
    let before = missing_neighbor.entries.clone();
    assert!(matches!(
        lists.preflight_remove(&missing_neighbor, class, page(2)),
        Err(PreflightError::Resolve(()))
    ));
    assert_eq!(missing_neighbor.entries, before);
    assert_eq!(lists.head(class), Some(page(1)));
}

#[test]
fn remove_preflight_resolver_count_is_independent_of_list_length() {
    let class = crate::require_some(SlabClass::new(0));
    let short = FakeStore::list(8, class);
    let mut short_lists = lists_with_head(&short, class, page(1));
    let _permit = crate::require_ok(short_lists.preflight_remove(&short, class, page(5)));
    let short_count = short.resolutions.get();

    let long = FakeStore::list(10_000, class);
    let mut long_lists = lists_with_head(&long, class, page(1));
    long.reset_resolutions();
    let _permit = crate::require_ok(long_lists.preflight_remove(&long, class, page(5_000)));

    assert_eq!(short_count, 4);
    assert_eq!(long.resolutions.get(), short_count);
}

#[test]
fn insert_and_head_removal_plans_update_only_the_typed_root() {
    let class = crate::require_some(SlabClass::new(0));
    assert_eq!(class.raw(), 0);
    assert!(SlabPageId::new(u64::MAX).is_none());
    assert_eq!(page(1).physical(), hyper::mm::PAGE_SIZE);
    assert_eq!(
        SlabLink::from_raw(SlabLink::from_page(page(1)).raw()).decode(),
        Ok(Some(page(1)))
    );
    assert_eq!(
        SlabLink::from_raw(hyper::mm::PAGE_SIZE + 1).decode(),
        Err(InvalidTopology)
    );
    assert_eq!(SlabLink::NONE.decode(), Ok(None));
    assert_eq!(SlabPageId::new(hyper::mm::PAGE_SIZE + 1), None);
    assert_eq!(PartialSlabLists::<1>::new().class(1), None);
    assert_eq!(
        PartialSlabLists::<2>::new().class(1).map(SlabClass::index),
        Some(1)
    );

    let mut store = FakeStore::list(2, class);
    store.entries.push(Entry {
        page: page(3),
        node: PartialNode {
            class,
            links: PartialLinks::DETACHED,
            linked: false,
        },
    });
    let mut lists = lists_with_head(&store, class, page(1));
    assert_eq!(
        crate::require_ok(lists.preflight_head(&store, class)).head(),
        Some(page(1))
    );
    let insert = crate::require_ok(lists.preflight_insert(&store, class, page(3)));
    assert_eq!(insert.target(), page(3));
    assert_eq!(insert.old_head(), Some(page(1)));
    insert.commit();
    assert_eq!(lists.head(class), Some(page(3)));

    let mut original_lists = lists_with_head(&store, class, page(1));
    {
        let abandoned = crate::require_ok(original_lists.preflight_remove(&store, class, page(1)));
        assert_eq!(abandoned.target(), page(1));
    }
    assert_eq!(original_lists.head(class), Some(page(1)));

    let remove = crate::require_ok(original_lists.preflight_remove(&store, class, page(1)));
    assert_eq!(remove.target(), page(1));
    remove.commit();
    assert_eq!(original_lists.head(class), Some(page(2)));
}

#[test]
fn topology_exposes_borrow_bound_permits_not_detached_plans() {
    let source = include_str!("../../../../src/mm/allocator/heap/partial.rs");

    assert!(source.contains("struct HeadPermit<'a"));
    assert!(source.contains("struct InsertPermit<'a"));
    assert!(source.contains("struct RemovePermit<'a"));
    assert!(!source.contains("struct HeadSnapshot"));
    assert!(!source.contains("struct InsertPlan"));
    assert!(!source.contains("struct RemovePlan"));
    assert!(!source.contains("commit_insert"));
    assert!(!source.contains("commit_remove"));
}
