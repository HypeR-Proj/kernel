// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Link operations over caller-owned nodes and list endpoints.
//!
//! No container, allocation, lock, wakeup, reclamation or Drop policy is owned
//! here. Callers serialize links, enforce unique membership and decide when
//! returned nodes may be destroyed. Adapters select the link field, allowing
//! separate memberships without changing payload types.

/// An exclusively borrowed successor link. Implement on a node handle, e.g.
/// `Box<Node>`, rather than prescribing how nodes are allocated or retained.
pub trait NextLink: Sized {
    fn next(&self) -> &Option<Self>;
    fn next_mut(&mut self) -> &mut Option<Self>;
}

/// Inserts a detached node before the first matching entry; otherwise appends.
/// Returns the unchanged node if it already carries a successor. O(n), no
/// recursion. `before` must not mutate links or node membership.
pub fn insert_before<N: NextLink>(
    head: &mut Option<N>,
    node: N,
    mut before: impl FnMut(&N, &N) -> bool,
) -> Result<(), N> {
    if node.next().is_some() {
        return Err(node);
    }
    let mut link = head;
    loop {
        if link.as_ref().is_none_or(|current| before(&node, current)) {
            let mut node = node;
            *node.next_mut() = link.take();
            *link = Some(node);
            return Ok(());
        }
        link = match link.as_mut() {
            Some(current) => current.next_mut(),
            None => invariant(),
        };
    }
}

/// Unlinks the first match, returning it with a cleared successor. O(n).
pub fn remove_first<N: NextLink>(
    head: &mut Option<N>,
    mut matches: impl FnMut(&N) -> bool,
) -> Option<N> {
    let mut link = head;
    loop {
        if matches(link.as_ref()?) {
            return pop_front(link);
        }
        link = link.as_mut()?.next_mut();
    }
}

pub fn pop_front<N: NextLink>(head: &mut Option<N>) -> Option<N> {
    let mut node = head.take()?;
    *head = node.next_mut().take();
    Some(node)
}

/// Successor operations for lists that retain a second handle at their tail.
/// Clone retains the same node, not a copy of its payload. Callers establish
/// exclusive membership and their required lock context before mutation.
pub trait TailLink: Clone {
    fn link_successor(&self, next: Self);
    fn take_successor(&self) -> Option<Self>;
}

/// O(1) append of a detached node. No handle is cloned beyond the two handles
/// required for the existing head/tail representation.
pub fn push_back<N: TailLink>(head: &mut Option<N>, tail: &mut Option<N>, node: N) {
    check_ends(head, tail);
    if let Some(last) = tail.as_ref() {
        last.link_successor(node.clone());
    } else {
        *head = Some(node.clone());
    }
    *tail = Some(node);
}

pub fn pop_front_with_tail<N: TailLink>(head: &mut Option<N>, tail: &mut Option<N>) -> Option<N> {
    check_ends(head, tail);
    let node = head.take()?;
    *head = node.take_successor();
    if head.is_none() {
        *tail = None;
    }
    Some(node)
}

/// Moves all links from `other` to the tail without allocating or visiting
/// nodes. The two lists must have disjoint membership.
pub fn append<N: TailLink>(
    head: &mut Option<N>,
    tail: &mut Option<N>,
    other_head: &mut Option<N>,
    other_tail: &mut Option<N>,
) {
    check_ends(head, tail);
    check_ends(other_head, other_tail);
    let Some(first) = other_head.take() else {
        return;
    };
    if let Some(last) = tail.as_ref() {
        last.link_successor(first);
    } else {
        *head = Some(first);
    }
    *tail = other_tail.take();
}

fn check_ends<N>(head: &Option<N>, tail: &Option<N>) {
    if head.is_none() != tail.is_none() {
        invariant();
    }
}
#[cold]
fn invariant() -> ! {
    crate::debug::invariant_failure("linked list endpoints invariant")
}
