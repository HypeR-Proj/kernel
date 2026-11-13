// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared u64 word waits with independent domain-exit and cancellation sources.
//!
//! Lock order is word bucket -> domain -> Thread registry/node. Source teardown
//! and backing release occur after all source locks have been released.

use super::sync_domain::{self, Admission, SyncDomain};
use super::{Process, ProcessError};
use crate::accounting::{CommittedCharge, ResourceAmount, ResourceKind};
use crate::mm::user_space::{MemoryObjectError, NativeSharedAtomicWord, VmoObject};
use crate::object::{
    KernelRef, OperationPin, PreparedCancellation, TimedWaitPreparation, prepare_timed_wait,
};
use crate::task::{WaitOutcome, WaitQueue, WaitTicket, scheduler};
use hyper::sync::InterruptSpinLock;

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[derive(Debug)]
pub(crate) enum Error {
    InvalidInput,
    Process(ProcessError),
    Memory(MemoryObjectError),
    Wait(crate::object::ObjectWaitError),
    Domain(sync_domain::Error),
}
impl From<ProcessError> for Error {
    fn from(value: ProcessError) -> Self {
        Self::Process(value)
    }
}
impl From<MemoryObjectError> for Error {
    fn from(value: MemoryObjectError) -> Self {
        Self::Memory(value)
    }
}
impl From<crate::object::ObjectWaitError> for Error {
    fn from(value: crate::object::ObjectWaitError) -> Self {
        Self::Wait(value)
    }
}
impl From<sync_domain::Error> for Error {
    fn from(value: sync_domain::Error) -> Self {
        Self::Domain(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Recheck,
    OwnerExited(u64),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Key {
    domain: u64,
    backing: u64,
    offset: u64,
}
impl Key {
    pub(crate) fn new(domain: u64, backing: u64, offset: u64) -> Self {
        Self {
            domain,
            backing,
            offset,
        }
    }
    fn bucket(self) -> &'static Lock<Bucket> {
        let hash = self.domain.rotate_left(17) ^ self.backing.rotate_left(31) ^ (self.offset / 8);
        &BUCKETS[hash as usize % BUCKETS.len()]
    }
}

pub(crate) struct Word {
    word: NativeSharedAtomicWord,
    _vmo: KernelRef<VmoObject, OperationPin>,
    key: Key,
    _charge: CommittedCharge,
}
impl Word {
    pub(crate) fn prepare(
        process: &Process,
        vmo: KernelRef<VmoObject, OperationPin>,
        domain: u64,
        offset: u64,
    ) -> Result<Self, Error> {
        vmo.object().validate_shared_atomic_u64_offset(offset)?;
        let (backing, absolute) = vmo.object().sync_key(domain, offset)?;
        let charge = process
            .resource_domain()
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::PinnedPages, 1)
                    .with(
                        ResourceKind::KernelMemoryBytes,
                        core::mem::size_of::<Node>() as u64,
                    ),
            )
            .map_err(ProcessError::from)?
            .commit();
        let word = vmo.object().pin_shared_atomic_u64(offset)?;
        Ok(Self {
            word,
            _vmo: vmo,
            key: Key::new(domain, backing, absolute),
            _charge: charge,
        })
    }
    fn load(&self) -> u64 {
        let address = match crate::mm::memory::linear_address(self.word.physical()) {
            Some(address) => address,
            None => invariant(),
        };
        // SAFETY: admission retains an aligned eight-byte RAM word, independent
        // of handle/unmap lifetime. The machine load permits malicious peer
        // accesses without creating Rust references to their writable bytes.
        unsafe {
            crate::hal::atomic::load_external_u64(core::ptr::with_exposed_provenance(address))
        }
    }
}

struct Node {
    word: Word,
    ticket: WaitTicket,
    next: Option<WaitTicket>,
}
pub(crate) struct ThreadWordWaiter {
    node: Lock<Option<Node>>,
}
impl ThreadWordWaiter {
    pub(crate) const fn new() -> Self {
        Self {
            node: Lock::new(None),
        }
    }
    pub(crate) fn is_idle(&self) -> bool {
        self.node.with(|node| node.is_none())
    }
    fn install(&self, node: Node) {
        self.node.with(|slot| {
            if slot.is_some() {
                invariant();
            }
            *slot = Some(node);
        });
    }
    fn with<R>(&self, ticket: WaitTicket, f: impl FnOnce(&mut Node) -> R) -> R {
        self.node.with(|node| match node {
            Some(node) if node.ticket == ticket => f(node),
            _ => invariant(),
        })
    }
    fn remove(&self, ticket: WaitTicket) -> Node {
        self.node.with(|node| match node.take() {
            Some(node) if node.ticket == ticket => node,
            _ => invariant(),
        })
    }
}
impl Drop for ThreadWordWaiter {
    fn drop(&mut self) {
        if !self.is_idle() {
            invariant();
        }
    }
}
fn with_node<R>(ticket: WaitTicket, f: impl FnOnce(&ThreadWordWaiter) -> R) -> R {
    match scheduler::with_wait_context(ticket.thread(), |context| f(&context.sync_word)) {
        Ok(result) => result,
        Err(_) => invariant(),
    }
}
struct Bucket {
    queue: WaitQueue,
    head: Option<WaitTicket>,
    tail: Option<WaitTicket>,
}
static BUCKETS: [Lock<Bucket>; 64] = [const {
    Lock::new(Bucket {
        queue: WaitQueue::new(),
        head: None,
        tail: None,
    })
}; 64];
impl Bucket {
    fn insert(&mut self, node: Node) {
        let ticket = node.ticket;
        with_node(ticket, |slot| slot.install(node));
        if let Some(tail) = self.tail {
            with_node(tail, |slot| {
                slot.with(tail, |node| node.next = Some(ticket))
            });
        } else {
            self.head = Some(ticket);
        }
        self.tail = Some(ticket);
    }
    fn remove(&mut self, target: WaitTicket) -> Node {
        let mut current = self.head;
        let mut previous = None;
        while let Some(ticket) = current {
            let next = with_node(ticket, |slot| slot.with(ticket, |node| node.next));
            if ticket == target {
                if let Some(previous) = previous {
                    with_node(previous, |slot| {
                        slot.with(previous, |node| node.next = next)
                    });
                } else {
                    self.head = next;
                }
                if self.tail == Some(ticket) {
                    self.tail = previous;
                }
                return with_node(ticket, |slot| slot.remove(ticket));
            }
            previous = Some(ticket);
            current = next;
        }
        invariant()
    }
}

pub(crate) fn wait(
    process: &Process,
    domain: KernelRef<SyncDomain, OperationPin>,
    word: Word,
    request: crate::abi::native::SyncWaitRequest,
    cancellation: Option<PreparedCancellation>,
    cancelled: impl Fn() -> bool,
) -> Result<Outcome, Error> {
    wait_inner(
        process,
        domain,
        word,
        request,
        cancellation,
        cancelled,
        #[cfg(all(
            feature = "kernel-test",
            feature = "kernel-self-test",
            CONFIG_ARCH_AARCH64
        ))]
        |_| {},
    )
}

/// Exercises source arbitration at the enrollment/condition-recheck boundary.
/// Production builds contain neither the hook nor this entry point.
#[cfg(all(
    feature = "kernel-test",
    feature = "kernel-self-test",
    CONFIG_ARCH_AARCH64
))]
pub(crate) fn wait_with_admission_hook(
    process: &Process,
    domain: KernelRef<SyncDomain, OperationPin>,
    word: Word,
    request: crate::abi::native::SyncWaitRequest,
    cancellation: Option<PreparedCancellation>,
    cancelled: impl Fn() -> bool,
    hook: impl FnOnce(WaitTicket),
) -> Result<Outcome, Error> {
    wait_inner(
        process,
        domain,
        word,
        request,
        cancellation,
        cancelled,
        hook,
    )
}

fn wait_inner(
    process: &Process,
    domain: KernelRef<SyncDomain, OperationPin>,
    word: Word,
    request: crate::abi::native::SyncWaitRequest,
    cancellation: Option<PreparedCancellation>,
    cancelled: impl Fn() -> bool,
    #[cfg(all(
        feature = "kernel-test",
        feature = "kernel-self-test",
        CONFIG_ARCH_AARCH64
    ))]
    before_recheck: impl FnOnce(WaitTicket),
) -> Result<Outcome, Error> {
    let owner = domain.object().owner_query(request.owner);
    if owner == sync_domain::OwnerState::Invalid {
        return Err(Error::InvalidInput);
    }
    if word.load() != request.expected {
        return Ok(Outcome::Recheck);
    }
    if owner == sync_domain::OwnerState::Dead {
        return Ok(Outcome::OwnerExited(request.owner));
    }
    let prepared_domain = domain.object().prepare_wait(&process.resource_domain())?;
    let prepared = match prepare_timed_wait(&process.resource_domain(), request.deadline)? {
        TimedWaitPreparation::Completed(WaitOutcome::TimedOut) => return Ok(Outcome::TimedOut),
        TimedWaitPreparation::Completed(_) => invariant(),
        TimedWaitPreparation::Armed(prepared) => prepared,
    };
    let ticket = prepared.ticket();
    let cancellation = cancellation.map(|source| source.register(ticket));
    #[cfg(all(
        feature = "kernel-test",
        feature = "kernel-self-test",
        CONFIG_ARCH_AARCH64
    ))]
    before_recheck(ticket);
    let bucket = word.key.bucket();
    enum Publication {
        Immediate(
            crate::object::PreparedTimedWait,
            Result<Outcome, Error>,
            sync_domain::RejectedWait,
        ),
        Waiting(crate::object::PublishedTimedWait, sync_domain::DomainWait),
    }
    let mut word = Some(word);
    // SAFETY: the bucket IRQ mask crosses directly into the exact park or is
    // dropped before local completion. Domain admission occurs under the bucket,
    // closing both the compare/wake and owner-exit/registration races.
    let (publication, mask) = unsafe {
        bucket.with_mask_retained(|state| {
            let admission =
                SyncDomain::register_wait(&domain, prepared_domain, ticket, request.owner, || {
                    match word.as_ref() {
                        Some(word) => word.load() == request.expected,
                        None => invariant(),
                    }
                });
            let registered = match admission {
                Admission::Recheck(rejected) => {
                    return Publication::Immediate(prepared, Ok(Outcome::Recheck), rejected);
                }
                Admission::OwnerExited(rejected) => {
                    return Publication::Immediate(
                        prepared,
                        Ok(Outcome::OwnerExited(request.owner)),
                        rejected,
                    );
                }
                Admission::InvalidOwner(rejected) => {
                    return Publication::Immediate(prepared, Err(Error::InvalidInput), rejected);
                }
                Admission::Registered(registration) => registration,
            };
            if cancelled() {
                prepared.request_cancellation();
            }
            let word = match word.take() {
                Some(word) => word,
                None => invariant(),
            };
            state.insert(Node {
                word,
                ticket,
                next: None,
            });
            Publication::Waiting(prepared.publish_locked(&state.queue), registered)
        })
    };
    match publication {
        Publication::Immediate(prepared, outcome, rejected) => {
            drop(mask);
            drop(rejected);
            if cancelled() {
                prepared.request_cancellation();
            }
            let retired = prepared.complete_unpublished();
            let event_cancelled = cancellation.is_some_and(|source| source.finish());
            match retired? {
                WaitOutcome::TimedOut => Ok(Outcome::TimedOut),
                WaitOutcome::Cancelled => Ok(Outcome::Cancelled),
                WaitOutcome::Notified if event_cancelled => Ok(Outcome::Cancelled),
                WaitOutcome::Notified => outcome,
            }
        }
        Publication::Waiting(published, domain_wait) => {
            let (outcome, retirement) = published.finish(mask);
            domain_wait.finish();
            let cancelled = cancellation.is_some_and(|source| source.finish());
            let node = bucket.with(|state| state.remove(ticket));
            drop(node);
            retirement?;
            Ok(match outcome {
                WaitOutcome::Notified if cancelled => Outcome::Cancelled,
                WaitOutcome::Notified => Outcome::Recheck,
                WaitOutcome::TimedOut => Outcome::TimedOut,
                WaitOutcome::Cancelled => Outcome::Cancelled,
            })
        }
    }
}

pub(crate) fn wake(key: Key, count: u32) -> Result<u64, Error> {
    key.bucket().with(|state| {
        let mut current = state.head;
        let mut count_done = 0;
        while let Some(ticket) = current {
            if count_done == u64::from(count) {
                break;
            }
            let (candidate, next) = with_node(ticket, |slot| {
                slot.with(ticket, |node| (node.word.key, node.next))
            });
            current = next;
            if candidate == key
                && scheduler::notify_registered_with(ticket, || {})
                    .map_err(ProcessError::from)?
                    .won
            {
                count_done += 1;
            }
        }
        Ok(count_done)
    })
}
fn invariant() -> ! {
    hyper::debug::invariant_failure("shared u64 wait lifetime invariant")
}
