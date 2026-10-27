// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free AVL index over scheduler-owned nodes.
//!
//! The registry retains ownership of every node. The CPU lock must cover keys,
//! links and the root together; changing a queued key requires remove/reinsert.

// A 24-bit registry slot namespace cannot contain the 24,157,816 nodes
// required for AVL height 35. Forty entries include the removal successor path
// and bound corrupted traversal without imposing a configured thread limit.
const MAX_PATH: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Links<I> {
    pub left: Option<I>,
    pub right: Option<I>,
    pub height: u8,
}

impl<I> Links<I> {
    pub(crate) const EMPTY: Self = Self {
        left: None,
        right: None,
        height: 1,
    };
}

pub(crate) trait Nodes {
    type Id: Copy + Eq;
    type Key: Ord;
    type Error;
    fn key(&mut self, id: Self::Id) -> Result<Self::Key, Self::Error>;
    fn links(&mut self, id: Self::Id) -> Result<Links<Self::Id>, Self::Error>;
    fn set_links(&mut self, id: Self::Id, links: Links<Self::Id>) -> Result<(), Self::Error>;
    fn corrupt(&self) -> Self::Error;
}

#[derive(Clone, Copy)]
pub(crate) struct Tree<I> {
    root: Option<I>,
}

impl<I: Copy + Eq> Tree<I> {
    pub(crate) const fn new() -> Self {
        Self { root: None }
    }
    #[cfg(test)]
    pub(crate) const fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub(crate) fn first<N: Nodes<Id = I>>(&self, nodes: &mut N) -> Result<Option<I>, N::Error> {
        let Some(mut id) = self.root else {
            return Ok(None);
        };
        for _ in 0..MAX_PATH {
            match nodes.links(id)?.left {
                Some(left) => id = left,
                None => return Ok(Some(id)),
            }
        }
        Err(nodes.corrupt())
    }

    pub(crate) fn contains<N: Nodes<Id = I>>(
        &self,
        nodes: &mut N,
        id: I,
    ) -> Result<bool, N::Error> {
        let key = nodes.key(id)?;
        let mut current = self.root;
        for _ in 0..MAX_PATH {
            let Some(node) = current else {
                return Ok(false);
            };
            match key.cmp(&nodes.key(node)?) {
                core::cmp::Ordering::Equal => return Ok(node == id),
                core::cmp::Ordering::Less => current = nodes.links(node)?.left,
                core::cmp::Ordering::Greater => current = nodes.links(node)?.right,
            }
        }
        Err(nodes.corrupt())
    }

    /// Failure after mutation denotes corrupted CPU-owned state; callers must
    /// fail-stop instead of publishing a partially changed queue.
    pub(crate) fn insert<N: Nodes<Id = I>>(
        &mut self,
        nodes: &mut N,
        id: I,
        scratch: &mut MutationScratch<I>,
    ) -> Result<(), N::Error> {
        scratch.reset();
        let key = nodes.key(id)?;
        self.root = Some(insert_at(nodes, self.root, id, &key, scratch)?);
        Ok(())
    }

    // Keep the emitted stack budget auditable for both queue authorities.
    #[inline(never)]
    pub(crate) fn remove<N: Nodes<Id = I>>(
        &mut self,
        nodes: &mut N,
        id: I,
        scratch: &mut MutationScratch<I>,
    ) -> Result<(), N::Error> {
        scratch.reset();
        let key = nodes.key(id)?;
        self.root = remove_at(nodes, self.root, id, &key, scratch)?;
        nodes.set_links(id, Links::EMPTY)?;
        Ok(())
    }
}

fn height<N: Nodes>(nodes: &mut N, id: Option<N::Id>) -> Result<u8, N::Error> {
    match id {
        Some(id) => Ok(nodes.links(id)?.height),
        None => Ok(0),
    }
}

fn update<N: Nodes>(nodes: &mut N, id: N::Id, mut links: Links<N::Id>) -> Result<(), N::Error> {
    links.height = height(nodes, links.left)?
        .max(height(nodes, links.right)?)
        .checked_add(1)
        .ok_or_else(|| nodes.corrupt())?;
    nodes.set_links(id, links)
}

fn rotate_left<N: Nodes>(nodes: &mut N, id: N::Id) -> Result<N::Id, N::Error> {
    let mut links = nodes.links(id)?;
    let right = links.right.ok_or_else(|| nodes.corrupt())?;
    let mut replacement = nodes.links(right)?;
    links.right = replacement.left;
    update(nodes, id, links)?;
    replacement.left = Some(id);
    update(nodes, right, replacement)?;
    Ok(right)
}

fn rotate_right<N: Nodes>(nodes: &mut N, id: N::Id) -> Result<N::Id, N::Error> {
    let mut links = nodes.links(id)?;
    let left = links.left.ok_or_else(|| nodes.corrupt())?;
    let mut replacement = nodes.links(left)?;
    links.left = replacement.right;
    update(nodes, id, links)?;
    replacement.right = Some(id);
    update(nodes, left, replacement)?;
    Ok(left)
}

#[inline(never)] // Audited separately from the bounded mutation scratch.
fn balance<N: Nodes>(nodes: &mut N, id: N::Id) -> Result<N::Id, N::Error> {
    let mut links = nodes.links(id)?;
    let delta = i16::from(height(nodes, links.left)?) - i16::from(height(nodes, links.right)?);
    if delta > 1 {
        let left = links.left.ok_or_else(|| nodes.corrupt())?;
        let child = nodes.links(left)?;
        if height(nodes, child.left)? < height(nodes, child.right)? {
            links.left = Some(rotate_left(nodes, left)?);
            nodes.set_links(id, links)?;
        }
        rotate_right(nodes, id)
    } else if delta < -1 {
        let right = links.right.ok_or_else(|| nodes.corrupt())?;
        let child = nodes.links(right)?;
        if height(nodes, child.right)? < height(nodes, child.left)? {
            links.right = Some(rotate_right(nodes, right)?);
            nodes.set_links(id, links)?;
        }
        rotate_left(nodes, id)
    } else {
        update(nodes, id, links)?;
        Ok(id)
    }
}

/// Reusable mutation path, exclusively borrowed under the owning queue lock.
///
/// The caller owns its storage; kernel queues allocate it before publication.
/// Only entries below `len` are live, so a seed identity avoids an extra
/// `Option<Id>` discriminant without any uninitialized reads.
pub(crate) struct MutationScratch<I> {
    ids: [I; MAX_PATH],
    right: u64,
    len: usize,
}

impl<I: Copy> MutationScratch<I> {
    pub(crate) const fn new(seed: I) -> Self {
        Self {
            ids: [seed; MAX_PATH],
            right: 0,
            len: 0,
        }
    }

    fn reset(&mut self) {
        self.len = 0;
        self.right = 0;
    }

    fn push<N: Nodes<Id = I>>(&mut self, nodes: &N, id: I, right: bool) -> Result<(), N::Error> {
        let slot = self.ids.get_mut(self.len).ok_or_else(|| nodes.corrupt())?;
        *slot = id;
        let bit = 1_u64 << self.len;
        self.right = (self.right & !bit) | if right { bit } else { 0 };
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<(I, bool)> {
        self.len = self.len.checked_sub(1)?;
        Some((self.ids[self.len], self.right & (1_u64 << self.len) != 0))
    }
}

/// Reconnect and balance a replaced subtree up to an earlier path boundary.
/// Two-child removal first repairs the successor path, then its original
/// ancestors, using this same scratch array for both segments.
#[inline(never)] // Preserve the image stack audit boundary below mutation.
fn rebuild<N: Nodes>(
    nodes: &mut N,
    path: &mut MutationScratch<N::Id>,
    mut subtree: Option<N::Id>,
    stop: usize,
) -> Result<Option<N::Id>, N::Error> {
    while path.len > stop {
        let (parent, right) = path.pop().ok_or_else(|| nodes.corrupt())?;
        let mut links = nodes.links(parent)?;
        if right {
            links.right = subtree;
        } else {
            links.left = subtree;
        }
        nodes.set_links(parent, links)?;
        subtree = Some(balance(nodes, parent)?);
    }
    Ok(subtree)
}

fn insert_at<N: Nodes>(
    nodes: &mut N,
    mut root: Option<N::Id>,
    id: N::Id,
    key: &N::Key,
    path: &mut MutationScratch<N::Id>,
) -> Result<N::Id, N::Error> {
    while let Some(parent) = root {
        let right = match key.cmp(&nodes.key(parent)?) {
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Equal => return Err(nodes.corrupt()),
        };
        path.push(nodes, parent, right)?;
        let links = nodes.links(parent)?;
        root = if right { links.right } else { links.left };
    }
    // Reject duplicates and overlong paths before changing an existing node.
    nodes.set_links(id, Links::EMPTY)?;
    rebuild(nodes, path, Some(id), 0)?.ok_or_else(|| nodes.corrupt())
}

fn remove_at<N: Nodes>(
    nodes: &mut N,
    mut root: Option<N::Id>,
    id: N::Id,
    key: &N::Key,
    path: &mut MutationScratch<N::Id>,
) -> Result<Option<N::Id>, N::Error> {
    let links = loop {
        let parent = root.ok_or_else(|| nodes.corrupt())?;
        let links = nodes.links(parent)?;
        let right = match key.cmp(&nodes.key(parent)?) {
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Equal if parent == id => break links,
            core::cmp::Ordering::Equal => return Err(nodes.corrupt()),
        };
        path.push(nodes, parent, right)?;
        root = if right { links.right } else { links.left };
    };
    let replacement = match (links.left, links.right) {
        (None, right) => right,
        (left, None) => left,
        (Some(left), Some(mut successor)) => {
            let boundary = path.len;
            let successor_links = loop {
                let child = nodes.links(successor)?;
                let Some(next) = child.left else {
                    break child;
                };
                path.push(nodes, successor, false)?;
                successor = next;
            };
            let right = rebuild(nodes, path, successor_links.right, boundary)?;
            nodes.set_links(
                successor,
                Links {
                    left: Some(left),
                    right,
                    height: 1,
                },
            )?;
            Some(balance(nodes, successor)?)
        }
    };
    rebuild(nodes, path, replacement, 0)
}
