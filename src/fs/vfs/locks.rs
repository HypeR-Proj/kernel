// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native advisory locks: canonical node contention, per-open authority.
//!
//! The domain lock precedes scheduler wait arbitration and never acquires a
//! filesystem mutex. Owner atomics only permit safe shared storage; all state
//! transitions are serialized by the matching domain lock. Waiters retain an
//! owner record, not its `FileObject` or node, so the queue creates no owner cycle.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use hyper::mm::{FallibleArc, try_box};
use hyper::sync::InterruptSpinLock;

use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::ObjectWaitError, crate::object::PreparedTimedWait,
    crate::object::PublishedTimedWait, crate::object::TimedWaitPreparation,
    crate::object::prepare_timed_wait,
};
use {
    crate::task::WaitOutcome, crate::task::WaitQueue, crate::task::WaitTicket,
    crate::task::scheduler,
};

pub(crate) use super::lock_state::LockMode;

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[path = "lock_self_test.rs"]
mod self_test;
use super::lock_state::{Admission, Grants, OwnerState};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) use self_test::run as run_self_test;

// Bounds the work done by authority retirement with interrupts masked. Waiter
// memory is also charged to the requesting domain before publication.
const MAX_WAITERS: usize = 256;
static NEXT_DOMAIN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) enum LockError {
    Allocation,
    Resource(ResourceError),
    Wait(ObjectWaitError),
    WrongDomain,
    Closed,
    Busy,
    WouldBlock,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockOwnerError {
    Allocation,
    IdentifierExhausted,
    Resource(ResourceError),
}

impl From<ResourceError> for LockError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<ObjectWaitError> for LockError {
    fn from(error: ObjectWaitError) -> Self {
        Self::Wait(error)
    }
}

struct Owner {
    state: AtomicU8,
    _charge: CommittedCharge,
}

impl Owner {
    fn load(&self) -> OwnerState {
        match OwnerState::from_bits(self.state.load(Ordering::Relaxed)) {
            Some(state) => state,
            None => invariant(),
        }
    }

    fn store(&self, state: OwnerState) {
        self.state.store(state.bits(), Ordering::Relaxed);
    }
}

pub(crate) struct LockOwner {
    domain: u64,
    owner: FallibleArc<Owner>,
}

struct Waiter {
    owner: FallibleArc<Owner>,
    mode: LockMode,
    ticket: WaitTicket,
    completed: bool,
    next: Option<Box<Self>>,
    _charge: CommittedCharge,
}

struct State {
    identity: u64,
    grants: Grants,
    queue: WaitQueue,
    waiters: Option<Box<Waiter>>,
    waiter_count: usize,
}

pub(crate) struct FileLocks {
    state: InterruptSpinLock<State, crate::hal::irq::LocalMask>,
}

impl FileLocks {
    pub(crate) const fn new() -> Self {
        Self {
            state: InterruptSpinLock::new(State {
                identity: 0,
                grants: Grants::new(),
                queue: WaitQueue::new(),
                waiters: None,
                waiter_count: 0,
            }),
        }
    }

    pub(crate) fn create_owner(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<LockOwner, LockOwnerError> {
        let bytes = u64::try_from(FallibleArc::<Owner>::allocation_size())
            .map_err(|_| LockOwnerError::Allocation)?;
        let charge = sponsor
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))
            .map_err(LockOwnerError::Resource)?
            .commit();
        let owner = FallibleArc::try_new(Owner {
            state: AtomicU8::new(OwnerState::new().bits()),
            _charge: charge,
        })
        .map_err(|_| LockOwnerError::Allocation)?;
        let domain = self.state.with(|state| {
            if state.identity == 0 {
                state.identity = NEXT_DOMAIN
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| LockOwnerError::IdentifierExhausted)?;
            }
            Ok::<_, LockOwnerError>(state.identity)
        })?;
        Ok(LockOwner { domain, owner })
    }

    pub(crate) fn lock(
        &self,
        owner: &LockOwner,
        mode: LockMode,
        deadline: u64,
        sponsor: &ResourceDomain,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), LockError> {
        let first = self.state.with(|state| {
            state.validate(owner)?;
            if cancelled() {
                return Err(LockError::Cancelled);
            }
            let admission = state.request(&owner.owner, mode);
            state.reconcile();
            Ok(admission)
        })?;
        if admission_result(first, deadline == 0)? {
            return Ok(());
        }

        let charge = reserve(sponsor, core::mem::size_of::<Waiter>())?;
        let mut storage = try_box(core::mem::MaybeUninit::<Waiter>::uninit())
            .map_err(|_| LockError::Allocation)?;
        let prepared = match prepare_timed_wait(sponsor, deadline)? {
            TimedWaitPreparation::Completed(outcome) => return outcome_result(outcome),
            TimedWaitPreparation::Armed(prepared) => prepared,
        };
        let ticket = prepared.ticket();
        storage.write(Waiter {
            owner: owner.owner.clone(),
            mode,
            ticket,
            completed: false,
            next: None,
            _charge: charge,
        });
        // SAFETY: the complete Waiter value was initialized above. Its owner
        // record is retained independently; this operation pins the file node.
        let mut waiter = Some(unsafe { storage.assume_init() });
        enum Publication {
            Immediate(PreparedTimedWait, Result<(), LockError>),
            Waiting(PublishedTimedWait),
        }
        // SAFETY: condition admission, queue publication, close, and handoff
        // share this lock. The retained mask is either passed to finish or
        // released before aborting an unpublished wait.
        let (publication, mask) = unsafe {
            self.state.with_mask_retained(|state| {
                if cancelled() {
                    return Publication::Immediate(prepared, Err(LockError::Cancelled));
                }
                let mut current = owner.owner.load();
                let queued = state.has_pending();
                let admission = state.grants.classify(&current, mode, queued);
                if admission != Admission::Wait {
                    let result = admission_result(admission, false).map(|_| ());
                    if result.is_ok()
                        && scheduler::notify_registered_with(ticket, || {
                            state.grants.request(&mut current, mode, queued);
                            owner.owner.store(current);
                        })
                        .is_err()
                    {
                        invariant();
                    }
                    state.reconcile();
                    return Publication::Immediate(prepared, result);
                }
                if state.waiter_count == MAX_WAITERS {
                    return Publication::Immediate(prepared, Err(LockError::Busy));
                }
                current.set_pending(true);
                owner.owner.store(current);
                let node = match waiter.take() {
                    Some(node) => node,
                    None => invariant(),
                };
                state.push(node);
                Publication::Waiting(prepared.publish_locked(&state.queue))
            })
        };
        match publication {
            Publication::Immediate(prepared, result) => {
                drop(mask);
                let outcome = prepared.abort()?;
                result.and_then(|()| outcome_result(outcome))
            }
            Publication::Waiting(published) => {
                let (outcome, retirement) = published.finish(mask);
                let removed = self.state.with(|state| {
                    let removed = state.remove(ticket);
                    let mut current = owner.owner.load();
                    current.set_pending(false);
                    owner.owner.store(current);
                    state.reconcile();
                    removed
                });
                drop(removed);
                retirement?;
                outcome_result(outcome)
            }
        }
    }

    pub(crate) fn unlock(&self, owner: &LockOwner) -> Result<(), LockError> {
        self.state.with(|state| {
            state.validate(owner)?;
            let mut current = owner.owner.load();
            if current.closed() {
                return Err(LockError::Closed);
            }
            state.grants.release(&mut current);
            owner.owner.store(current);
            state.cancel(&owner.owner);
            state.reconcile();
            Ok(())
        })
    }

    /// Final active authority teardown, independent of resolved operation pins.
    pub(crate) fn close(&self, owner: &LockOwner) {
        self.state.with(|state| {
            if state.validate(owner).is_err() {
                invariant();
            }
            let mut current = owner.owner.load();
            state.grants.close(&mut current);
            owner.owner.store(current);
            state.cancel(&owner.owner);
            state.reconcile();
        });
    }
}

impl State {
    fn validate(&self, owner: &LockOwner) -> Result<(), LockError> {
        if owner.domain == self.identity && self.identity != 0 {
            Ok(())
        } else {
            Err(LockError::WrongDomain)
        }
    }

    fn request(&mut self, owner: &Owner, mode: LockMode) -> Admission {
        let queued = self.has_pending();
        let mut current = owner.load();
        let result = self.grants.request(&mut current, mode, queued);
        owner.store(current);
        result
    }

    fn has_pending(&self) -> bool {
        let mut next = self.waiters.as_deref();
        while let Some(waiter) = next {
            if !waiter.completed {
                return true;
            }
            next = waiter.next.as_deref();
        }
        false
    }

    fn push(&mut self, waiter: Box<Waiter>) {
        let mut link = &mut self.waiters;
        while let Some(node) = link {
            link = &mut node.next;
        }
        *link = Some(waiter);
        self.waiter_count += 1;
    }

    fn remove(&mut self, ticket: WaitTicket) -> Box<Waiter> {
        let mut link = &mut self.waiters;
        loop {
            if link.as_ref().is_some_and(|node| node.ticket == ticket) {
                let Some(mut removed) = link.take() else {
                    invariant();
                };
                *link = removed.next.take();
                self.waiter_count -= 1;
                return removed;
            }
            link = match link.as_mut() {
                Some(node) => &mut node.next,
                None => invariant(),
            };
        }
    }

    fn cancel(&mut self, owner: &Owner) {
        let mut next = self.waiters.as_deref_mut();
        while let Some(waiter) = next {
            if core::ptr::eq(&*waiter.owner, owner) && !waiter.completed {
                if scheduler::resolve_wait(waiter.ticket, WaitOutcome::Cancelled).is_err() {
                    invariant();
                }
                waiter.completed = true;
            }
            next = waiter.next.as_deref_mut();
        }
    }

    fn reconcile(&mut self) {
        let mut next = self.waiters.as_deref_mut();
        while let Some(waiter) = next {
            if !waiter.completed {
                let mut current = waiter.owner.load();
                if current.closed() {
                    invariant();
                }
                if !self.grants.compatible(waiter.mode) {
                    break;
                }
                let result = scheduler::notify_registered_with(waiter.ticket, || {
                    self.grants.grant(&mut current, waiter.mode);
                    waiter.owner.store(current);
                });
                if result.is_err() {
                    invariant();
                }
                // A lost arbitration means timeout/cancellation already won.
                // Its continuation still owns queue removal and accounting.
                waiter.completed = true;
            }
            next = waiter.next.as_deref_mut();
        }
    }
}

fn admission_result(admission: Admission, nonblocking: bool) -> Result<bool, LockError> {
    match admission {
        Admission::Granted => Ok(true),
        Admission::Wait if nonblocking => Err(LockError::WouldBlock),
        Admission::Wait => Ok(false),
        Admission::Busy if nonblocking => Err(LockError::WouldBlock),
        Admission::Busy => Err(LockError::Busy),
        Admission::Closed => Err(LockError::Closed),
    }
}

fn outcome_result(outcome: WaitOutcome) -> Result<(), LockError> {
    match outcome {
        WaitOutcome::Notified => Ok(()),
        WaitOutcome::TimedOut => Err(LockError::TimedOut),
        WaitOutcome::Cancelled => Err(LockError::Cancelled),
    }
}

fn reserve(sponsor: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, LockError> {
    let bytes = u64::try_from(bytes).map_err(|_| LockError::Allocation)?;
    Ok(sponsor
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))?
        .commit())
}

#[cold]
fn invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: file lock lifecycle invariant failed"))
}
