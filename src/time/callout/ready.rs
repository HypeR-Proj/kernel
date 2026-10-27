// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free delivery and retirement FIFOs under the source timer lock.

use core::ptr::NonNull;

use super::lifecycle::Core;

#[derive(Default)]
pub(super) struct Links {
    previous: Option<NonNull<Core>>,
    next: Option<NonNull<Core>>,
    linked: bool,
}

#[derive(Clone, Copy)]
pub(super) enum WorkKind {
    Callback,
    Wake,
}

struct List {
    head: Option<NonNull<Core>>,
    tail: Option<NonNull<Core>>,
}

pub(crate) struct ReadyQueue {
    callbacks: List,
    wakes: List,
    prefer_wake: bool,
}

// SAFETY: all links are protected by the source CPU timer lock. Every list
// membership retains a separate Core reference until removal under that lock.
unsafe impl Send for ReadyQueue {}

impl ReadyQueue {
    pub(crate) const fn new() -> Self {
        Self {
            callbacks: List::new(),
            wakes: List::new(),
            prefer_wake: true,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.callbacks.head.is_none() && self.wakes.head.is_none()
    }

    /// Caller holds the source queue lock and the Ready execution reference.
    pub(super) unsafe fn push(&mut self, core: NonNull<Core>) {
        // SAFETY: caller establishes source-lock and execution ownership.
        unsafe { self.callbacks.push(core, WorkKind::Callback) };
    }

    /// Caller holds the source lock and owns the exact Ready membership.
    pub(super) unsafe fn remove(&mut self, core: NonNull<Core>) {
        // SAFETY: caller establishes source-lock and membership ownership.
        unsafe { self.callbacks.remove(core, WorkKind::Callback) };
    }

    /// Caller holds the source lock and has published the retained wake owner.
    pub(super) unsafe fn push_wake(&mut self, core: NonNull<Core>) {
        // SAFETY: caller establishes source-lock and wake ownership.
        unsafe { self.wakes.push(core, WorkKind::Wake) };
    }

    /// Claim and membership removal must remain in the same source-lock scope.
    pub(super) unsafe fn pop(&mut self) -> Option<(NonNull<Core>, WorkKind)> {
        let kind =
            if self.wakes.head.is_some() && (self.prefer_wake || self.callbacks.head.is_none()) {
                WorkKind::Wake
            } else {
                WorkKind::Callback
            };
        let list = match kind {
            WorkKind::Wake => &mut self.wakes,
            WorkKind::Callback => &mut self.callbacks,
        };
        let core = list.head?;
        // SAFETY: head remains a retained current member under the source lock.
        unsafe { list.remove(core, kind) };
        self.prefer_wake = matches!(kind, WorkKind::Callback);
        Some((core, kind))
    }
}

impl List {
    const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    unsafe fn push(&mut self, core: NonNull<Core>, kind: WorkKind) {
        // SAFETY: source lock protects links; membership owns a Core reference.
        let links = unsafe { links(core, kind) };
        if links.linked {
            hyper::debug::invariant_failure("callout linked twice into work queue");
        }
        links.previous = self.tail;
        links.next = None;
        links.linked = true;
        if let Some(tail) = self.tail {
            // SAFETY: tail is another retained member of this locked list.
            unsafe { links_mut(tail, kind, |tail| tail.next = Some(core)) };
        } else {
            self.head = Some(core);
        }
        self.tail = Some(core);
    }

    unsafe fn remove(&mut self, core: NonNull<Core>, kind: WorkKind) {
        // SAFETY: source lock protects links; membership owns a Core reference.
        let links = unsafe { links(core, kind) };
        if !links.linked {
            hyper::debug::invariant_failure("callout missing from work queue");
        }
        if let Some(previous) = links.previous {
            // SAFETY: previous is a distinct retained member under this lock.
            unsafe { links_mut(previous, kind, |peer| peer.next = links.next) };
        } else {
            self.head = links.next;
        }
        if let Some(next) = links.next {
            // SAFETY: next is a distinct retained member under this lock.
            unsafe { links_mut(next, kind, |peer| peer.previous = links.previous) };
        } else {
            self.tail = links.previous;
        }
        *links = Links::default();
    }
}

unsafe fn links<'a>(core: NonNull<Core>, kind: WorkKind) -> &'a mut Links {
    // SAFETY: the caller holds the source timer lock and retains Core for this
    // borrow; only the selected UnsafeCell's contents are mutably borrowed.
    let core = unsafe { core.as_ref() };
    let cell = match kind {
        WorkKind::Callback => &core.ready,
        WorkKind::Wake => &core.wake,
    };
    // SAFETY: the same source lock provides exclusive selected-link access.
    unsafe { &mut *cell.get() }
}

unsafe fn links_mut(core: NonNull<Core>, kind: WorkKind, update: impl FnOnce(&mut Links)) {
    // SAFETY: the caller proves this is a retained, independently borrowed peer.
    update(unsafe { links(core, kind) });
}
