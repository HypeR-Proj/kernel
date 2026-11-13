// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scoped thread-death observation for userspace-owned shared mutexes.
//!
//! Lock order: caller word bucket -> domain state -> node links. Thread JOIN
//! takes its membership list before domain state. Death and domain retirement
//! detach one list first and visit its opposite endpoint only after unlocking.
//! No kernel path interprets mutex state or owns application recovery policy.

use super::{UserThread, user_thread::UserThreadObject};
use crate::accounting::{
    CommittedCharge, ResourceAmount, ResourceDomain, ResourceError, ResourceKind,
};
use crate::authority::Rights;
use crate::object::{
    KernelObject, KernelRef, KernelService, ObjectKind, OperationPin, TransferClass, WeakKernelRef,
    object_allocation_size, private,
};
use crate::task::{WaitSource, WaitSourceRegistration, WaitTicket, scheduler};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hyper::mm::{FallibleArc, WeakFallibleArc};
use hyper::sync::InterruptSpinLock;

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;
mod tokens;
pub(crate) use tokens::OwnerState;
use tokens::Tokens;
const DISPATCH_BATCH: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Allocation,
    InvalidInput,
    ConflictingBinding,
    AdmissionClosed,
    Exhausted,
    Resource(ResourceError),
}
impl From<ResourceError> for Error {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

struct Membership {
    token: AtomicU64,
    domain_id: u64,
    domain: WeakKernelRef<SyncDomain>,
    thread: WeakKernelRef<UserThreadObject>,
    domain_next: Lock<Option<FallibleArc<Self>>>,
    thread_next: Lock<Option<FallibleArc<Self>>>,
    _charge: CommittedCharge,
}

struct ThreadState {
    closed: bool,
    head: Option<FallibleArc<Membership>>,
}

pub(super) struct ThreadMemberships {
    state: Lock<ThreadState>,
}
impl ThreadMemberships {
    pub(super) const fn new() -> Self {
        Self {
            state: Lock::new(ThreadState {
                closed: false,
                head: None,
            }),
        }
    }

    pub(super) fn detach_all(&self) {
        let mut current = self.state.with(|state| {
            state.closed = true;
            state.head.take()
        });
        while let Some(node) = current {
            current = node.thread_next.with(Option::take);
            if let Some(domain) = node.domain.upgrade() {
                SyncDomain::publish_death(&domain, node.token.load(Ordering::Acquire));
            }
            // A failed upgrade transfers cleanup to the queued domain finalizer.
            // The Thread edge has already been detached in either case.
            drop(node);
        }
    }

    fn unlink(&self, target: &FallibleArc<Membership>) {
        let removed = self
            .state
            .with(|state| unlink_membership(&mut state.head, target, false));
        drop(removed);
    }
}

fn unlink_membership(
    head: &mut Option<FallibleArc<Membership>>,
    target: &FallibleArc<Membership>,
    domain: bool,
) -> Option<FallibleArc<Membership>> {
    let mut current = head.clone();
    let mut previous: Option<FallibleArc<Membership>> = None;
    while let Some(node) = current {
        let link = if domain {
            &node.domain_next
        } else {
            &node.thread_next
        };
        if core::ptr::eq(&*node, &**target) {
            let next = link.with(Option::take);
            if let Some(previous) = previous {
                let previous_link = if domain {
                    &previous.domain_next
                } else {
                    &previous.thread_next
                };
                previous_link.with(|slot| *slot = next);
            } else {
                *head = next;
            }
            return Some(node);
        }
        current = link.with(|next| next.clone());
        previous = Some(node);
    }
    None
}

struct State {
    tokens: Tokens,
    death_epoch: u64,
    members: Option<FallibleArc<Membership>>,
    wait_head: Option<FallibleArc<WaitNode>>,
    wait_tail: Option<FallibleArc<WaitNode>>,
    work_queued: bool,
}

pub(crate) struct SyncDomain {
    state: Lock<State>,
    work_next: Lock<Option<KernelRef<Self, KernelService>>>,
    _charge: CommittedCharge,
}

fn charge(sponsor: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, Error> {
    let bytes = u64::try_from(bytes).map_err(|_| Error::Allocation)?;
    Ok(sponsor
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))?
        .commit())
}

impl SyncDomain {
    pub(crate) fn try_new(sponsor: &ResourceDomain) -> Result<Self, Error> {
        let bytes = object_allocation_size::<Self>().ok_or(Error::Allocation)?;
        let charge = sponsor
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, bytes as u64)
                    .with(ResourceKind::KernelObjects, 1),
            )?
            .commit();
        Ok(Self {
            state: Lock::new(State {
                tokens: Tokens::new(),
                death_epoch: 0,
                members: None,
                wait_head: None,
                wait_tail: None,
                work_queued: false,
            }),
            work_next: Lock::new(None),
            _charge: charge,
        })
    }

    pub(crate) fn join(
        domain: &KernelRef<Self, OperationPin>,
        thread: &UserThread,
        sponsor: &ResourceDomain,
    ) -> Result<u64, Error> {
        // The fast repeated-JOIN observation never allocates or consumes quota.
        let existing = thread.sync_memberships().state.with(|state| {
            if state.closed {
                return Err(Error::AdmissionClosed);
            }
            let mut current = state.head.clone();
            while let Some(node) = current {
                if node.domain_id == domain.koid().get() {
                    return Ok(Some(node.token.load(Ordering::Acquire)));
                }
                current = node.thread_next.with(|next| next.clone());
            }
            Ok(None)
        })?;
        if let Some(token) = existing {
            return Ok(token);
        }
        let charge = charge(sponsor, FallibleArc::<Membership>::allocation_size())?;
        let prepared = FallibleArc::try_new(Membership {
            token: AtomicU64::new(0),
            domain_id: domain.koid().get(),
            domain: domain.downgrade(),
            thread: thread.weak_object(),
            domain_next: Lock::new(None),
            thread_next: Lock::new(None),
            _charge: charge,
        })
        .map_err(|_| Error::Allocation)?;
        thread.sync_memberships().state.with(|thread_state| {
            if thread_state.closed {
                return Err(Error::AdmissionClosed);
            }
            let mut current = thread_state.head.clone();
            while let Some(node) = current {
                if node.domain_id == domain.koid().get() {
                    return Ok(node.token.load(Ordering::Acquire));
                }
                current = node.thread_next.with(|next| next.clone());
            }
            domain.object().state.with(|state| {
                let token = state.tokens.publish().ok_or(Error::Exhausted)?;
                // Every fallible operation precedes the joint high-water/list publication.
                prepared.token.store(token, Ordering::Release);
                prepared
                    .thread_next
                    .with(|next| *next = thread_state.head.take());
                prepared
                    .domain_next
                    .with(|next| *next = state.members.take());
                thread_state.head = Some(prepared.clone());
                state.members = Some(prepared.clone());
                Ok(token)
            })
        })
    }

    pub(crate) fn owner_query(&self, token: u64) -> OwnerState {
        self.state.with(|state| state.owner(token))
    }

    fn publish_death(domain: &KernelRef<Self, KernelService>, token: u64) {
        let (removed, enqueue) = domain.object().state.with(|state| {
            let mut current = state.members.clone();
            while let Some(node) = current {
                if node.token.load(Ordering::Relaxed) == token {
                    let removed = unlink_membership(&mut state.members, &node, true);
                    state.death_epoch = match state.death_epoch.checked_add(1) {
                        Some(epoch) => epoch,
                        None => invariant(),
                    };
                    let enqueue = !state.work_queued && state.wait_head.is_some();
                    if enqueue {
                        state.work_queued = true;
                    }
                    return (removed, enqueue);
                }
                current = node.domain_next.with(|next| next.clone());
            }
            (None, false)
        });
        // The Release unlock publishes all writes consumed by execution detach;
        // query/admission acquire this same lock before reporting owner death.
        drop(removed);
        if enqueue {
            enqueue_work(domain.clone());
        }
    }

    pub(crate) fn bind(
        domain: &KernelRef<Self, OperationPin>,
        vmo: &crate::mm::user_space::VmoObject,
        offset: u64,
        length: u64,
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        let storage = vmo.writable().ok_or(Error::InvalidInput)?;
        let end = offset.checked_add(length).ok_or(Error::InvalidInput)?;
        if length == 0
            || !offset.is_multiple_of(8)
            || !length.is_multiple_of(8)
            || end > storage.size()
        {
            return Err(Error::InvalidInput);
        }
        struct BindingLifetime {
            _domain: KernelRef<SyncDomain, KernelService>,
            _charge: CommittedCharge,
        }
        let bytes = core::mem::size_of::<BindingLifetime>()
            + core::mem::size_of::<crate::mm::user_space::SyncBinding>();
        let charge = charge(sponsor, bytes)?;
        let owner = hyper::mm::try_box(BindingLifetime {
            _domain: domain.retain_service(),
            _charge: charge,
        })
        .map_err(|_| Error::Allocation)?;
        let binding = hyper::mm::try_box(crate::mm::user_space::SyncBinding::new(
            domain.koid().get(),
            offset,
            end,
            owner,
        ))
        .map_err(|_| Error::Allocation)?;
        match storage.bind_sync(binding) {
            Ok(redundant) => {
                drop(redundant);
                Ok(())
            }
            Err(rejected) => {
                drop(rejected);
                Err(Error::ConflictingBinding)
            }
        }
    }

    pub(crate) fn prepare_wait(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<PreparedDomainWait, Error> {
        let charge = charge(sponsor, FallibleArc::<WaitNode>::allocation_size())?;
        let node = FallibleArc::try_new(WaitNode {
            links: Lock::new(WaitLinks {
                next: None,
                prev: None,
                linked: false,
                epoch: 0,
                ticket: None,
            }),
            dispatching: AtomicBool::new(false),
            _charge: charge,
        })
        .map_err(|_| Error::Allocation)?;
        Ok(PreparedDomainWait { node })
    }

    pub(crate) fn register_wait(
        domain: &KernelRef<Self, OperationPin>,
        prepared: PreparedDomainWait,
        ticket: WaitTicket,
        observed_owner: u64,
        matches_expected: impl FnOnce() -> bool,
    ) -> Admission {
        let source = WaitSourceRegistration::new(ticket, WaitSource::SyncDomain);
        let outcome = domain.object().state.with(|state| {
            if !matches_expected() {
                return AdmissionKind::Recheck;
            }
            match state.owner(observed_owner) {
                OwnerState::Invalid => return AdmissionKind::Invalid,
                OwnerState::Dead => return AdmissionKind::OwnerExited,
                OwnerState::Live => {}
            }
            prepared.node.links.with(|links| {
                links.epoch = state.death_epoch;
                links.ticket = Some(ticket);
                links.linked = true;
                links.prev = state.wait_tail.as_ref().map(FallibleArc::downgrade);
            });
            if let Some(tail) = &state.wait_tail {
                tail.links
                    .with(|links| links.next = Some(prepared.node.clone()));
            } else {
                state.wait_head = Some(prepared.node.clone());
            }
            state.wait_tail = Some(prepared.node.clone());
            AdmissionKind::Registered
        });
        match outcome {
            AdmissionKind::Registered => Admission::Registered(DomainWait {
                domain: domain.retain_service(),
                node: prepared.node,
                source: Some(source),
                finished: false,
            }),
            AdmissionKind::Recheck => Admission::Recheck(RejectedWait {
                _prepared: prepared,
                _source: source,
            }),
            AdmissionKind::OwnerExited => Admission::OwnerExited(RejectedWait {
                _prepared: prepared,
                _source: source,
            }),
            AdmissionKind::Invalid => Admission::InvalidOwner(RejectedWait {
                _prepared: prepared,
                _source: source,
            }),
        }
    }

    fn retire(&self) {
        let mut current = self.state.with(|state| {
            if state.wait_head.is_some() || state.work_queued {
                invariant();
            }
            state.members.take()
        });
        while let Some(node) = current {
            current = node.domain_next.with(Option::take);
            if let Some(thread) = node.thread.upgrade() {
                thread.object().sync_memberships().unlink(&node);
            }
            drop(node);
        }
    }
}

impl State {
    fn owner(&self, token: u64) -> OwnerState {
        self.tokens.observe(token, || {
            let mut current = self.members.clone();
            while let Some(node) = current {
                if node.token.load(Ordering::Relaxed) == token {
                    return true;
                }
                current = node.domain_next.with(|next| next.clone());
            }
            false
        })
    }

    fn unlink_wait(&mut self, node: &FallibleArc<WaitNode>) {
        let links = node.links.with(|links| {
            if !links.linked {
                return None;
            }
            links.linked = false;
            Some((links.prev.take(), links.next.take()))
        });
        let Some((previous, next)) = links else {
            return;
        };
        let previous = previous.and_then(|previous| previous.upgrade());
        if let Some(previous) = &previous {
            previous.links.with(|links| links.next = next.clone());
        } else {
            self.wait_head = next.clone();
        }
        if let Some(next) = &next {
            next.links
                .with(|links| links.prev = previous.as_ref().map(FallibleArc::downgrade));
        } else {
            self.wait_tail = previous;
        }
    }
}

struct WaitLinks {
    next: Option<FallibleArc<WaitNode>>,
    prev: Option<WeakFallibleArc<WaitNode>>,
    linked: bool,
    epoch: u64,
    ticket: Option<WaitTicket>,
}
struct WaitNode {
    links: Lock<WaitLinks>,
    dispatching: AtomicBool,
    _charge: CommittedCharge,
}
pub(crate) struct PreparedDomainWait {
    node: FallibleArc<WaitNode>,
}
enum AdmissionKind {
    Registered,
    Recheck,
    OwnerExited,
    Invalid,
}
pub(crate) struct RejectedWait {
    _prepared: PreparedDomainWait,
    _source: WaitSourceRegistration,
}
pub(crate) enum Admission {
    Registered(DomainWait),
    Recheck(RejectedWait),
    OwnerExited(RejectedWait),
    InvalidOwner(RejectedWait),
}

#[must_use = "finish domain source before reusing its scheduler ticket"]
pub(crate) struct DomainWait {
    domain: KernelRef<SyncDomain, KernelService>,
    node: FallibleArc<WaitNode>,
    source: Option<WaitSourceRegistration>,
    finished: bool,
}
impl DomainWait {
    pub(crate) fn finish(mut self) {
        self.domain
            .object()
            .state
            .with(|state| state.unlink_wait(&self.node));
        // Dispatch owns only the exact ticket; after detaching the queue entry
        // no new dispatcher can claim this node. Join its bounded notification
        // before releasing the source obligation and allowing ticket reuse.
        while self.node.dispatching.load(Ordering::Acquire) {
            // The dispatcher retains its local IRQ mask from extraction until
            // every bounded notification completes. It cannot be descheduled
            // behind this waiter on the same CPU, and no lock is held here.
            core::hint::spin_loop();
        }
        drop(self.source.take());
        self.finished = true;
    }
}
impl Drop for DomainWait {
    fn drop(&mut self) {
        if !self.finished {
            invariant();
        }
    }
}

struct WorkQueue {
    head: Option<KernelRef<SyncDomain, KernelService>>,
    tail: Option<KernelRef<SyncDomain, KernelService>>,
}
static WORK: Lock<WorkQueue> = Lock::new(WorkQueue {
    head: None,
    tail: None,
});
fn enqueue_work(domain: KernelRef<SyncDomain, KernelService>) {
    WORK.with(|queue| {
        if let Some(tail) = &queue.tail {
            tail.object().work_next.with(|next| {
                if next.is_some() {
                    invariant();
                }
                *next = Some(domain.clone());
            });
        } else {
            queue.head = Some(domain.clone());
        }
        queue.tail = Some(domain);
    });
    crate::reaper::request();
}
pub(crate) fn work_pending() -> bool {
    WORK.with(|queue| queue.head.is_some())
}

/// A finite FIFO batch; domain death never performs scheduler fanout while
/// holding the domain IRQ lock. New arrivals carry the current death epoch and
/// cannot move ahead of registrations covered by the captured cut.
pub(crate) fn dispatch_one_batch() -> bool {
    let domain = WORK.with(|queue| {
        let domain = queue.head.take()?;
        queue.head = domain.object().work_next.with(Option::take);
        if queue.head.is_none() {
            queue.tail = None;
        }
        Some(domain)
    });
    let Some(domain) = domain else {
        return false;
    };
    let mut batch: [Option<FallibleArc<WaitNode>>; DISPATCH_BATCH] = core::array::from_fn(|_| None);
    // SAFETY: the mask is retained only for a fixed batch of nonblocking
    // scheduler notifications, outside the domain lock. It prevents a woken
    // higher-priority waiter from preempting this CPU before its dispatch join
    // flag clears. Keep all extracted owners alive until after mask release.
    let (requeue, mask) = unsafe {
        domain.object().state.with_mask_retained(|state| {
            let cut = state.death_epoch;
            for slot in &mut batch {
                let Some(node) = state.wait_head.clone() else {
                    break;
                };
                if node.links.with(|links| links.epoch) >= cut {
                    break;
                }
                node.dispatching.store(true, Ordering::Release);
                state.unlink_wait(&node);
                *slot = Some(node);
            }
            let more = state
                .wait_head
                .as_ref()
                .is_some_and(|head| head.links.with(|links| links.epoch < cut));
            if !more {
                state.work_queued = false;
            }
            more
        })
    };
    for node in batch.iter().flatten() {
        let ticket = node.links.with(|links| links.ticket);
        let Some(ticket) = ticket else {
            invariant();
        };
        if scheduler::notify_registered_with(ticket, || {}).is_err() {
            invariant();
        }
        node.dispatching.store(false, Ordering::Release);
    }
    drop(mask);
    drop(batch);
    if requeue {
        enqueue_work(domain);
    }
    true
}

impl private::Sealed for SyncDomain {}
impl private::UserExportable for SyncDomain {}
impl KernelObject for SyncDomain {
    const KIND: ObjectKind = ObjectKind::SYNC_DOMAIN;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::SYNC_WAIT)
        .union(Rights::SYNC_WAKE)
        .union(Rights::SYNC_CONFIGURE)
        .union(Rights::SYNC_JOIN);
    fn on_final_reap(&self) {
        self.retire();
    }
}
fn invariant() -> ! {
    hyper::debug::invariant_failure("sync domain lifecycle invariant")
}
