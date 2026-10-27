// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Mutable ordered index with preallocated nodes. Rotations, deletion, and
//! successor lookup allocate nothing, including during memory-pressure cleanup.

use alloc::boxed::Box;

type Link<K, V> = Option<Box<Node<K, V>>>;

pub(super) struct Node<K, V> {
    pub(super) key: K,
    pub(super) value: V,
    left: Link<K, V>,
    right: Link<K, V>,
    height: u32,
}

impl<K, V> Node<K, V> {
    pub(super) const fn new(key: K, value: V) -> Self {
        Self {
            key,
            value,
            left: None,
            right: None,
            height: 1,
        }
    }
}

pub(super) struct Tree<K, V>(Link<K, V>);

impl<K: Copy + Ord, V> Tree<K, V> {
    pub(super) const fn new() -> Self {
        Self(None)
    }

    pub(super) fn get(&self, key: K) -> Option<&V> {
        let mut node = self.0.as_deref();
        while let Some(current) = node {
            match key.cmp(&current.key) {
                core::cmp::Ordering::Less => node = current.left.as_deref(),
                core::cmp::Ordering::Greater => node = current.right.as_deref(),
                core::cmp::Ordering::Equal => return Some(&current.value),
            }
        }
        None
    }

    pub(super) fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let mut node = self.0.as_deref_mut();
        while let Some(current) = node {
            match key.cmp(&current.key) {
                core::cmp::Ordering::Less => node = current.left.as_deref_mut(),
                core::cmp::Ordering::Greater => node = current.right.as_deref_mut(),
                core::cmp::Ordering::Equal => return Some(&mut current.value),
            }
        }
        None
    }

    /// First key strictly after `previous`, or the first key for a new scan.
    pub(super) fn next(&self, previous: Option<K>) -> Option<(K, &V)> {
        let mut node = self.0.as_deref();
        let mut candidate = None;
        while let Some(current) = node {
            if previous.is_none_or(|key| current.key > key) {
                candidate = Some(current);
                node = current.left.as_deref();
            } else {
                node = current.right.as_deref();
            }
        }
        candidate.map(|node| (node.key, &node.value))
    }

    pub(super) fn insert(&mut self, node: Box<Node<K, V>>) {
        if node.left.is_some() || node.right.is_some() {
            invariant_failure();
        }
        self.0 = Some(insert(self.0.take(), node));
    }

    pub(super) fn remove(&mut self, key: K) -> Option<Box<Node<K, V>>> {
        let (root, removed) = remove(self.0.take(), key);
        self.0 = root;
        removed
    }

    #[cfg(test)]
    pub(super) fn height_for_test(&self) -> u32 {
        height(&self.0)
    }
}

fn height<K, V>(link: &Link<K, V>) -> u32 {
    link.as_ref().map_or(0, |node| node.height)
}

fn update<K, V>(node: &mut Node<K, V>) {
    node.height = 1 + height(&node.left).max(height(&node.right));
}

fn left<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let Some(mut top) = root.right.take() else {
        invariant_failure();
    };
    root.right = top.left.take();
    update(&mut root);
    top.left = Some(root);
    update(&mut top);
    top
}

fn right<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let Some(mut top) = root.left.take() else {
        invariant_failure();
    };
    root.left = top.right.take();
    update(&mut root);
    top.right = Some(root);
    update(&mut top);
    top
}

fn balance<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    update(&mut root);
    if height(&root.left) > height(&root.right) + 1 {
        let Some(child) = root.left.as_ref() else {
            invariant_failure();
        };
        if height(&child.right) > height(&child.left) {
            root.left = root.left.take().map(left);
        }
        return right(root);
    }
    if height(&root.right) > height(&root.left) + 1 {
        let Some(child) = root.right.as_ref() else {
            invariant_failure();
        };
        if height(&child.left) > height(&child.right) {
            root.right = root.right.take().map(right);
        }
        return left(root);
    }
    root
}

fn insert<K: Ord, V>(root: Link<K, V>, node: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let Some(mut root) = root else {
        return node;
    };
    match node.key.cmp(&root.key) {
        core::cmp::Ordering::Less => root.left = Some(insert(root.left.take(), node)),
        core::cmp::Ordering::Greater => root.right = Some(insert(root.right.take(), node)),
        core::cmp::Ordering::Equal => invariant_failure(),
    }
    balance(root)
}

fn take_first<K, V>(mut root: Box<Node<K, V>>) -> (Link<K, V>, Box<Node<K, V>>) {
    let Some(child) = root.left.take() else {
        let replacement = root.right.take();
        root.height = 1;
        return (replacement, root);
    };
    let (replacement, first) = take_first(child);
    root.left = replacement;
    (Some(balance(root)), first)
}

fn remove<K: Ord, V>(root: Link<K, V>, key: K) -> (Link<K, V>, Link<K, V>) {
    let Some(mut root) = root else {
        return (None, None);
    };
    let removed = match key.cmp(&root.key) {
        core::cmp::Ordering::Less => {
            let (replacement, removed) = remove(root.left.take(), key);
            root.left = replacement;
            removed
        }
        core::cmp::Ordering::Greater => {
            let (replacement, removed) = remove(root.right.take(), key);
            root.right = replacement;
            removed
        }
        core::cmp::Ordering::Equal => {
            let replacement = match (root.left.take(), root.right.take()) {
                (None, right) => right,
                (left, None) => left,
                (left, Some(right)) => {
                    let (right, mut successor) = take_first(right);
                    successor.left = left;
                    successor.right = right;
                    Some(balance(successor))
                }
            };
            root.height = 1;
            return (replacement, Some(root));
        }
    };
    (Some(balance(root)), removed)
}

impl<K, V> Drop for Tree<K, V> {
    fn drop(&mut self) {
        // Flatten before dropping owners: teardown uses constant stack space.
        while let Some(mut root) = self.0.take() {
            if let Some(mut child) = root.left.take() {
                root.left = child.right.take();
                child.right = Some(root);
                self.0 = Some(child);
            } else {
                self.0 = root.right.take();
            }
        }
    }
}

#[cold]
fn invariant_failure() -> ! {
    hyper::debug::invariant_failure("filesystem record index invariant")
}
