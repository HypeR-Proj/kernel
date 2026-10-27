// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Prepare dirent removal without copying or replacing RAM subtree identities.

use super::{Children, Entry, Error, FallibleArc, MutexGuard, Node, NodeKind};
use super::{ScratchBudget, ScratchVec, Timestamp, parent_hint, unlink_node};

struct Removal {
    node: FallibleArc<Node>,
    index: usize,
    retired: Option<Entry>,
}

/// Sources sharing one parent must share one children guard at commit.
pub(in crate::fs::vfs) struct Parent {
    node: FallibleArc<Node>,
    removals: ScratchVec<Removal>,
}

impl Parent {
    pub(in crate::fs::vfs) fn prepare(
        parents: &mut ScratchVec<Self>,
        source: &FallibleArc<Node>,
        budget: &ScratchBudget,
    ) -> Result<(), Error> {
        source.require(NodeKind::Directory)?;
        let parent = parent_hint(source).ok_or(Error::InvalidInput)?;
        let existing = parents
            .iter()
            .position(|entry| core::ptr::eq(&*entry.node, &*parent));
        let index = match existing {
            Some(index) => index,
            None => {
                parents.push(Self {
                    node: parent,
                    removals: ScratchVec::new(budget.clone()),
                })?;
                parents.len() - 1
            }
        };
        parents[index].removals.push(Removal {
            node: source.clone(),
            index: 0,
            retired: None,
        })?;
        Ok(())
    }

    /// The caller already holds every source filesystem's mutation guard.
    pub(in crate::fs::vfs) fn lock(&mut self) -> Result<LockedParent<'_>, Error> {
        let children = self.node.children.lock().map_err(Error::Lock)?;
        for removal in self.removals.iter_mut() {
            removal.index = children
                .entries
                .iter()
                .position(|entry| core::ptr::eq(&*entry.node, &*removal.node))
                .ok_or(Error::Busy)?;
        }
        self.removals
            .sort_unstable_by_key(|removal| core::cmp::Reverse(removal.index));
        if self
            .removals
            .windows(2)
            .any(|pair| pair[0].index == pair[1].index)
        {
            return Err(Error::InvalidInput);
        }
        Ok(LockedParent {
            node: &self.node,
            children,
            removals: &mut self.removals,
        })
    }
}

pub(in crate::fs::vfs) struct LockedParent<'a> {
    node: &'a Node,
    children: MutexGuard<'a, Children>,
    removals: &'a mut [Removal],
}

impl LockedParent<'_> {
    /// No allocation, sleeping lock or destructor may run during publication.
    /// Descending indices keep the remaining positions valid after `swap_remove`;
    /// readdir uses stable cookies, not the vector's physical order.
    pub(in crate::fs::vfs) fn detach(&mut self, now: Option<Timestamp>) {
        for removal in self.removals.iter_mut() {
            let entry = self.children.entries.swap_remove(removal.index);
            unlink_node(&entry.node);
            entry.node.changed(now);
            removal.retired = Some(entry);
        }
        self.node.modified(now);
    }
}
