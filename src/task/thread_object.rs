// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Durable kernel-object identity for scheduler-owned execution entities.
//!
//! A `ThreadObject` owns only stable identity and observation state. Kernel
//! stacks, architecture contexts, scheduling state, and execution payloads
//! remain in `task::thread::Thread` so retaining an object reference can never
//! retain a terminated Thread's heavyweight execution resources.

use crate::accounting::CommittedCharge;
use crate::authority::Rights;
use crate::process::UserThread;
use {
    crate::object::KernelObject, crate::object::KernelRef, crate::object::ObjectCreationError,
    crate::object::ObjectKind, crate::object::ObjectSnapshot,
    crate::object::Scheduler as SchedulerReference, crate::object::private,
};

pub(super) const THREAD_OBJECT_PAGE_CAPACITY: usize = 8;

/// Stable semantic role of one scheduler entity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadRole {
    Bootstrap,
    Idle,
    Kernel,
    User,
    Vcpu,
}

/// Authority-free identity returned by scheduler diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThreadObjectSnapshot {
    pub(crate) object: ObjectSnapshot,
    pub(crate) role: ThreadRole,
    pub(crate) process: Option<crate::process::ProcessId>,
}

/// Scheduler-registry phase associated with one diagnostic identity record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadObjectRegistryPhase {
    Resident,
    /// The identity record remains in the registry while the reaper owns the
    /// detached `Thread`. It is an authority-free generation record, not a
    /// promise that the underlying object is still live after heavy teardown.
    Retiring,
}

/// One scheduler identity to canonical-object diagnostic relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThreadObjectObservation {
    pub(crate) thread: super::thread::ThreadId,
    pub(crate) name: super::thread::ThreadNameSnapshot,
    pub(crate) object: ThreadObjectSnapshot,
    pub(crate) phase: ThreadObjectRegistryPhase,
    pub(crate) runtime_ns: u64,
}

/// Position in a bounded scheduler Thread-object scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThreadObjectScanCursor {
    pub(super) next_slot: usize,
}

impl ThreadObjectScanCursor {
    pub(crate) const fn from_token(token: usize) -> Self {
        Self { next_slot: token }
    }

    pub(crate) const fn token(self) -> usize {
        self.next_slot
    }
}

/// One allocation-free page of scheduler-to-object diagnostic relations.
pub(crate) struct ThreadObjectSnapshotPage {
    pub(super) entries: [Option<ThreadObjectObservation>; THREAD_OBJECT_PAGE_CAPACITY],
    pub(super) len: usize,
    pub(super) next: Option<ThreadObjectScanCursor>,
}

impl ThreadObjectSnapshotPage {
    pub(crate) const fn empty() -> Self {
        Self {
            entries: [None; THREAD_OBJECT_PAGE_CAPACITY],
            len: 0,
            next: None,
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &ThreadObjectObservation> {
        self.entries[..self.len].iter().filter_map(Option::as_ref)
    }

    pub(crate) const fn next(&self) -> Option<ThreadObjectScanCursor> {
        self.next
    }
}

pub(super) struct SystemThreadObject {
    tid: super::thread::ThreadId,
    role: ThreadRole,
    _object_charge: Option<CommittedCharge>,
}

impl private::Sealed for SystemThreadObject {}

impl KernelObject for SystemThreadObject {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        use {crate::object::diagnostics::DetailRecord, crate::object::diagnostics::Details};
        let role = match self.role {
            ThreadRole::Bootstrap => 1,
            ThreadRole::Idle => 2,
            ThreadRole::Kernel => 3,
            ThreadRole::User => 4,
            ThreadRole::Vcpu => 5,
        };
        Details::last(
            DetailRecord::Thread {
                tid: Some(self.tid.get()),
                role,
                phase: 0,
            },
            cursor,
        )
    }

    const KIND: ObjectKind = ObjectKind::THREAD;
    // Exportability is enforced by the absence of `private::UserExportable`.
    // Rights describe supported operations, not the authority boundary.
    const SUPPORTED_RIGHTS: Rights = Rights::INSPECT;
}

/// The one canonical object owner embedded in every scheduler Thread.
///
/// User Threads retain their existing `UserThreadObject`; the enum wraps that
/// same owner instead of constructing a second identity. System variants are
/// deliberately non-cloneable here so the scheduler owns exactly one durable
/// object reference unless a focused diagnostic operation takes a snapshot.
pub(super) enum ThreadObject {
    System(KernelRef<SystemThreadObject, SchedulerReference>),
    User(UserThread),
}

impl ThreadObject {
    pub(super) const fn system_allocation_size() -> Option<usize> {
        crate::object::object_allocation_size::<SystemThreadObject>()
    }

    pub(super) fn role(&self) -> ThreadRole {
        match self {
            Self::System(object) => object.object().role,
            Self::User(_) => ThreadRole::User,
        }
    }

    pub(super) fn try_system(
        tid: super::thread::ThreadId,
        role: ThreadRole,
    ) -> Result<Self, ObjectCreationError> {
        if role == ThreadRole::User {
            thread_object_invariant_violation();
        }
        KernelRef::try_new_scheduler(SystemThreadObject {
            tid,
            role,
            _object_charge: None,
        })
        .map(Self::System)
    }

    pub(super) fn try_accounted_system(
        tid: super::thread::ThreadId,
        role: ThreadRole,
        object_charge: CommittedCharge,
    ) -> Result<Self, ObjectCreationError> {
        if role == ThreadRole::User {
            thread_object_invariant_violation();
        }
        KernelRef::try_new_scheduler(SystemThreadObject {
            tid,
            role,
            _object_charge: Some(object_charge),
        })
        .map(Self::System)
    }

    pub(super) fn user(thread: UserThread) -> Self {
        Self::User(thread.into_scheduler_owner())
    }

    pub(super) fn snapshot(&self) -> ThreadObjectSnapshot {
        match self {
            Self::System(object) => ThreadObjectSnapshot {
                object: object.snapshot(),
                role: object.object().role,
                process: None,
            },
            Self::User(thread) => ThreadObjectSnapshot {
                object: thread.object_snapshot(),
                role: ThreadRole::User,
                process: Some(thread.process_id()),
            },
        }
    }

    pub(super) fn user_thread(&self) -> Option<&UserThread> {
        match self {
            Self::User(thread) => Some(thread),
            Self::System(_) => None,
        }
    }
}

#[cold]
fn thread_object_invariant_violation() -> ! {
    hyper::debug::invariant_failure(
        "task::thread_object::thread_object_invariant_violation invariant",
    )
}
