// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Left-leaning red-black ownership links and subtree deadline augmentation.
//!
//! Tree order uses immutable timer identity. Cached minima use extended time,
//! not wrapping counter comparisons, so their reduction is associative.
//! Rotations move owning boxes without moving allocations. Deletion extracts
//! the requested allocation, including when its successor replaces it in the
//! tree; callback and recycle ownership are never exchanged between nodes.

use super::{DeadlineKey, TimerNode, queue_invariant_failure};
use alloc::boxed::Box;

type Link = Option<Box<TimerNode>>;

fn red(link: &Link) -> bool {
    link.as_ref().is_some_and(|node| node.red)
}

fn left_red(link: &Link) -> bool {
    link.as_ref().is_some_and(|node| red(&node.left))
}

fn key(node: &TimerNode) -> DeadlineKey {
    DeadlineKey {
        time: node.time,
        sequence: node.sequence,
        identity: node.identity,
    }
}

fn refresh(node: &mut TimerNode) {
    let mut first = key(node);
    if let Some(left) = &node.left {
        first = first.min(left.earliest);
    }
    if let Some(right) = &node.right {
        first = first.min(right.earliest);
    }
    node.earliest = first;
}

fn rotate_left(mut node: Box<TimerNode>) -> Box<TimerNode> {
    let Some(mut next) = node.right.take() else {
        queue_invariant_failure()
    };
    node.right = next.left.take();
    next.red = node.red;
    node.red = true;
    refresh(&mut node);
    next.left = Some(node);
    refresh(&mut next);
    next
}

fn rotate_right(mut node: Box<TimerNode>) -> Box<TimerNode> {
    let Some(mut next) = node.left.take() else {
        queue_invariant_failure()
    };
    node.left = next.right.take();
    next.red = node.red;
    node.red = true;
    refresh(&mut node);
    next.right = Some(node);
    refresh(&mut next);
    next
}

fn flip_colors(node: &mut TimerNode) {
    let (Some(left), Some(right)) = (&mut node.left, &mut node.right) else {
        queue_invariant_failure()
    };
    node.red = !node.red;
    left.red = !left.red;
    right.red = !right.red;
}

fn balance(mut node: Box<TimerNode>) -> Box<TimerNode> {
    if red(&node.right) {
        node = rotate_left(node);
    }
    finish_balance(node)
}

fn finish_balance(mut node: Box<TimerNode>) -> Box<TimerNode> {
    if red(&node.left) && left_red(&node.left) {
        node = rotate_right(node);
    }
    if red(&node.left) && red(&node.right) {
        flip_colors(&mut node);
    }
    refresh(&mut node);
    node
}

pub(super) fn insert(root: Link, mut added: Box<TimerNode>) -> Box<TimerNode> {
    if added.left.is_some() || added.right.is_some() {
        queue_invariant_failure();
    }
    added.red = true;
    refresh(&mut added);
    let mut root = insert_at(root, added);
    root.red = false;
    root
}

fn insert_at(root: Link, added: Box<TimerNode>) -> Box<TimerNode> {
    let Some(mut node) = root else { return added };
    match added.identity.cmp(&node.identity) {
        core::cmp::Ordering::Less => node.left = Some(insert_at(node.left.take(), added)),
        core::cmp::Ordering::Greater => node.right = Some(insert_at(node.right.take(), added)),
        core::cmp::Ordering::Equal => queue_invariant_failure(),
    }
    // An insertion can finish a four-node directly by flipping its colors.
    // Deletion's unconditional right-red rotation would rotate twice first.
    if red(&node.right) && !red(&node.left) {
        node = rotate_left(node);
    }
    finish_balance(node)
}

pub(super) fn find(root: &Link, identity: u64) -> Option<&TimerNode> {
    let mut link = root;
    loop {
        let node = link.as_ref()?;
        match identity.cmp(&node.identity) {
            core::cmp::Ordering::Less => link = &node.left,
            core::cmp::Ordering::Greater => link = &node.right,
            core::cmp::Ordering::Equal => return Some(node),
        }
    }
}

pub(super) fn reset(root: &mut Link, identity: u64, deadline: u64, time: u128, sequence: u64) {
    let Some(node) = root else {
        queue_invariant_failure()
    };
    match identity.cmp(&node.identity) {
        core::cmp::Ordering::Less => reset(&mut node.left, identity, deadline, time, sequence),
        core::cmp::Ordering::Greater => reset(&mut node.right, identity, deadline, time, sequence),
        core::cmp::Ordering::Equal => {
            node.deadline = deadline;
            node.time = time;
            node.sequence = sequence;
        }
    }
    refresh(node);
}

fn move_red_left(mut node: Box<TimerNode>) -> Box<TimerNode> {
    flip_colors(&mut node);
    if left_red(&node.right) {
        let Some(right) = node.right.take() else {
            queue_invariant_failure()
        };
        node.right = Some(rotate_right(right));
        node = rotate_left(node);
        flip_colors(&mut node);
    }
    node
}

fn move_red_right(mut node: Box<TimerNode>) -> Box<TimerNode> {
    flip_colors(&mut node);
    if left_red(&node.left) {
        node = rotate_right(node);
        flip_colors(&mut node);
    }
    node
}

pub(super) fn remove(root: Link, identity: u64) -> (Link, Box<TimerNode>) {
    let Some(mut root) = root else {
        queue_invariant_failure()
    };
    if !red(&root.left) && !red(&root.right) {
        root.red = true;
    }
    let (mut root, mut removed) = remove_at(root, identity);
    if let Some(root) = &mut root {
        root.red = false;
    }
    if removed.left.is_some() || removed.right.is_some() {
        queue_invariant_failure();
    }
    removed.red = true;
    refresh(&mut removed);
    (root, removed)
}

fn remove_at(mut node: Box<TimerNode>, identity: u64) -> (Link, Box<TimerNode>) {
    let removed;
    if identity < node.identity {
        if !red(&node.left) && !left_red(&node.left) {
            node = move_red_left(node);
        }
        let Some(left) = node.left.take() else {
            queue_invariant_failure()
        };
        (node.left, removed) = remove_at(left, identity);
    } else {
        if red(&node.left) {
            node = rotate_right(node);
        }
        if identity == node.identity && node.right.is_none() {
            let left = node.left.take();
            return (left, node);
        }
        if !red(&node.right) && !left_red(&node.right) {
            node = move_red_right(node);
        }
        let Some(right) = node.right.take() else {
            queue_invariant_failure()
        };
        if identity == node.identity {
            let (right, mut successor) = remove_first(right);
            successor.left = node.left.take();
            successor.right = right;
            successor.red = node.red;
            removed = node;
            node = successor;
        } else {
            (node.right, removed) = remove_at(right, identity);
        }
    }
    (Some(balance(node)), removed)
}

fn remove_first(mut node: Box<TimerNode>) -> (Link, Box<TimerNode>) {
    if node.left.is_none() {
        let right = node.right.take();
        return (right, node);
    }
    if !red(&node.left) && !left_red(&node.left) {
        node = move_red_left(node);
    }
    let Some(left) = node.left.take() else {
        queue_invariant_failure()
    };
    let (left, removed) = remove_first(left);
    node.left = left;
    (Some(balance(node)), removed)
}

pub(super) fn validate(root: &Link, len: usize) -> bool {
    !red(root) && verify(root, None, None).is_some_and(|(_, count, _)| count == len)
}

/// Returns black height, count and recomputed earliest timestamp. This checks
/// the actual ownership tree, not a parallel model of balancing operations.
fn verify(
    root: &Link,
    lower: Option<u64>,
    upper: Option<u64>,
) -> Option<(usize, usize, Option<DeadlineKey>)> {
    let Some(node) = root else {
        return Some((1, 0, None));
    };
    if lower.is_some_and(|key| key >= node.identity)
        || upper.is_some_and(|key| key <= node.identity)
        || red(&node.right)
        || (node.red && red(&node.left))
    {
        return None;
    }
    let (left_black, left_count, left_min) = verify(&node.left, lower, Some(node.identity))?;
    let (right_black, right_count, right_min) = verify(&node.right, Some(node.identity), upper)?;
    let minimum = left_min
        .into_iter()
        .chain(right_min)
        .fold(key(node), DeadlineKey::min);
    if left_black != right_black || node.earliest != minimum {
        return None;
    }
    Some((
        left_black + usize::from(!node.red),
        left_count + right_count + 1,
        Some(minimum),
    ))
}
