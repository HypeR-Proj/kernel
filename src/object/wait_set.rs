// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Persistent, one-shot subscriptions with an allocation-free ready path.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hyper::mm::{FallibleArc, WeakFallibleArc, try_box};
use hyper::sync::InterruptSpinLock;

use crate::capability::ResolvedWaitable;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};

use super::signals::{PersistentObserver, SignalSnapshot};
use super::{
    KernelObject, ObjectKind, ObjectRetirement, ObjectWaitError, Rights, SignalMask, SignalSource,
    SignalState, SignalWaitOutcome, TransferClass, object_allocation_size, private,
};

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;
const MAX_CAPACITY: usize = 1024;
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) enum WaitSetError {
    Allocation,
    InvalidInput,
    Unsupported,
    Full,
    Missing,
    Busy,
    Closed,
    TimedOut,
    Resource(ResourceError),
    Wait(ObjectWaitError),
}

pub(crate) struct WaitSet {
    control: Lock<Control>,
    queue: FallibleArc<ReadyQueue>,
    _charge: CommittedCharge,
}

struct Control {
    closed: bool,
    capacity: usize,
    subscriptions: Vec<Subscription>,
}

struct Subscription {
    source: ResolvedWaitable,
    registration: FallibleArc<Registration>,
}

pub(super) struct Registration {
    id: u64,
    queue: WeakFallibleArc<ReadyQueue>,
    active: AtomicBool,
    pending: AtomicBool,
    _charge: CommittedCharge,
}

struct Ready {
    registration: FallibleArc<Registration>,
    signals: u64,
    sequence: u64,
}

struct ReadyQueue {
    state: Lock<QueueState>,
    signals: SignalState,
}

struct QueueState {
    closed: bool,
    capacity: usize,
    ready: VecDeque<Ready>,
}

/// An event remains pending until user copy succeeds. A fault restores it to
/// the bounded queue unless cancellation or final-handle close won the race.
pub(crate) struct Delivery {
    queue: FallibleArc<ReadyQueue>,
    ready: Option<Ready>,
}
impl Delivery {
    pub(crate) fn record(&self) -> [u8; 24] {
        let Some(ready) = &self.ready else {
            invariant()
        };
        let mut bytes = [0; 24];
        bytes[..8].copy_from_slice(&ready.registration.id.to_le_bytes());
        bytes[8..16].copy_from_slice(&ready.signals.to_le_bytes());
        bytes[16..].copy_from_slice(&ready.sequence.to_le_bytes());
        bytes
    }
    pub(crate) fn complete(mut self) {
        if let Some(ready) = self.ready.take() {
            ready.registration.pending.store(false, Ordering::Release);
        }
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        let mut ready = self.ready.take();
        self.queue.state.with(|state| {
            if state.closed
                || !ready
                    .as_ref()
                    .is_some_and(|ready| ready.registration.active.load(Ordering::Relaxed))
            {
                return;
            }
            if state.ready.len() >= state.capacity {
                invariant();
            }
            if let Some(ready) = ready.take() {
                state.ready.push_front(ready);
                self.queue.update(true);
            }
        });
        drop(ready);
    }
}

impl Registration {
    pub(super) const fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Called only under the source signal lock. The observer is disarmed
    /// before this callback, so each subscription occupies at most one slot.
    pub(super) fn notify(registration: &FallibleArc<Self>, snapshot: SignalSnapshot) {
        let Some(queue) = registration.queue.upgrade() else {
            return;
        };
        queue.state.with(|state| {
            if state.closed || !registration.active.load(Ordering::Relaxed) {
                return;
            }
            if state.ready.len() >= state.capacity
                || registration.pending.swap(true, Ordering::AcqRel)
            {
                invariant();
            }
            state.ready.push_back(Ready {
                registration: registration.clone(),
                signals: snapshot.signals().bits(),
                sequence: snapshot.sequence(),
            });
            queue.update(true);
        });
    }
}

impl ReadyQueue {
    const READABLE: SignalMask =
        SignalMask::from_trusted_bits(hyper::abi::native::HYPER_NATIVE_SIGNAL_WAIT_SET_READABLE);

    fn update(&self, readable: bool) {
        let (clear, set) = if readable {
            (SignalMask::EMPTY, Self::READABLE)
        } else {
            (Self::READABLE, SignalMask::EMPTY)
        };
        if self.signals.update(clear, set).is_err() {
            invariant();
        }
    }

    fn remove(&self, id: u64) {
        let removed = self.state.with(|state| {
            let removed = state
                .ready
                .iter()
                .position(|entry| entry.registration.id == id)
                .and_then(|index| state.ready.remove(index));
            self.update(state.closed || !state.ready.is_empty());
            removed
        });
        drop(removed);
    }
}

impl WaitSet {
    pub(crate) fn try_new(capacity: usize, domain: &ResourceDomain) -> Result<Self, WaitSetError> {
        if capacity == 0 || capacity > MAX_CAPACITY {
            return Err(WaitSetError::InvalidInput);
        }
        let bytes = object_allocation_size::<Self>()
            .and_then(|bytes| bytes.checked_add(FallibleArc::<ReadyQueue>::allocation_size()))
            .and_then(|bytes| {
                bytes.checked_add(
                    capacity
                        * (core::mem::size_of::<Subscription>() + core::mem::size_of::<Ready>()),
                )
            })
            .ok_or(WaitSetError::Allocation)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, bytes as u64),
            )
            .map_err(WaitSetError::Resource)?
            .commit();
        let mut subscriptions = Vec::new();
        subscriptions
            .try_reserve_exact(capacity)
            .map_err(|_| WaitSetError::Allocation)?;
        let mut ready = VecDeque::new();
        ready
            .try_reserve_exact(capacity)
            .map_err(|_| WaitSetError::Allocation)?;
        let queue = FallibleArc::try_new(ReadyQueue {
            state: Lock::new(QueueState {
                closed: false,
                capacity,
                ready,
            }),
            signals: SignalState::new(),
        })
        .map_err(|_| WaitSetError::Allocation)?;
        Ok(Self {
            control: Lock::new(Control {
                closed: false,
                capacity,
                subscriptions,
            }),
            queue,
            _charge: charge,
        })
    }

    pub(crate) fn add(
        &self,
        source: ResolvedWaitable,
        mask: u64,
        domain: &ResourceDomain,
    ) -> Result<u64, WaitSetError> {
        // No WaitSet-to-WaitSet edges: notification never recursively enters
        // another persistent queue and userspace cannot construct wait cycles.
        if source.kind() == ObjectKind::WAIT_SET {
            return Err(WaitSetError::InvalidInput);
        }
        if source.kind() == ObjectKind::CAPABILITY_CHANNEL {
            return Err(WaitSetError::Unsupported);
        }
        let mask = source
            .source()
            .validate(mask, false)
            .ok_or(WaitSetError::InvalidInput)?;
        let id = NEXT_REGISTRATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| WaitSetError::Full)?;
        let bytes = FallibleArc::<Registration>::allocation_size()
            + core::mem::size_of::<PersistentObserver>();
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
            .map_err(WaitSetError::Resource)?
            .commit();
        let registration = FallibleArc::try_new(Registration {
            id,
            queue: self.queue.downgrade(),
            active: AtomicBool::new(true),
            pending: AtomicBool::new(false),
            _charge: charge,
        })
        .map_err(|_| WaitSetError::Allocation)?;
        let observer = try_box(PersistentObserver::new(registration.clone(), mask))
            .map_err(|_| WaitSetError::Allocation)?;
        // Preparation owns allocations until admission. Taking them from this
        // Option avoids running allocator/accounting destructors under control.
        let mut prepared = Some((source, registration, observer));
        let result = self.control.with(|control| {
            if control.closed {
                return Err(WaitSetError::Closed);
            }
            if control.subscriptions.len() == control.capacity {
                return Err(WaitSetError::Full);
            }
            let Some((source, registration, observer)) = prepared.take() else {
                invariant()
            };
            source.source().state().subscribe(observer);
            control.subscriptions.push(Subscription {
                source,
                registration,
            });
            Ok(id)
        });
        drop(prepared);
        result
    }

    pub(crate) fn rearm(&self, id: u64) -> Result<(), WaitSetError> {
        self.control.with(|control| {
            if control.closed {
                return Err(WaitSetError::Closed);
            }
            let entry = control
                .subscriptions
                .iter()
                .find(|entry| entry.registration.id == id)
                .ok_or(WaitSetError::Missing)?;
            if entry.source.source().state().rearm_subscription(id) {
                Ok(())
            } else {
                Err(WaitSetError::Busy)
            }
        })
    }

    pub(crate) fn remove(&self, id: u64) -> Result<(), WaitSetError> {
        let detached = self.control.with(|control| {
            if control.closed {
                return Err(WaitSetError::Closed);
            }
            let index = control
                .subscriptions
                .iter()
                .position(|entry| entry.registration.id == id)
                .ok_or(WaitSetError::Missing)?;
            let entry = control.subscriptions.swap_remove(index);
            entry.registration.active.store(false, Ordering::Relaxed);
            let observer = entry.source.source().state().unsubscribe(id);
            self.queue.remove(id);
            Ok((entry, observer))
        })?;
        drop(detached);
        Ok(())
    }

    pub(crate) fn wait(
        &self,
        deadline: u64,
        domain: &ResourceDomain,
        cancelled: impl Fn() -> bool,
    ) -> Result<Delivery, WaitSetError> {
        loop {
            let ready = self.queue.state.with(|state| {
                if state.closed {
                    return Err(WaitSetError::Closed);
                }
                let ready = state.ready.pop_front();
                self.queue.update(!state.ready.is_empty());
                Ok(ready)
            })?;
            if let Some(ready) = ready {
                return Ok(Delivery {
                    queue: self.queue.clone(),
                    ready: Some(ready),
                });
            }
            match super::wait_one(
                SignalSource::new(&self.queue.signals, ReadyQueue::READABLE),
                domain,
                ReadyQueue::READABLE.bits(),
                deadline,
                &cancelled,
            )
            .map_err(WaitSetError::Wait)?
            {
                SignalWaitOutcome::Observed(_) => {}
                SignalWaitOutcome::TimedOut => return Err(WaitSetError::TimedOut),
                SignalWaitOutcome::Cancelled => return Err(WaitSetError::Closed),
            }
        }
    }

    fn close(&self) {
        let entries = self.control.with(|control| {
            control.closed = true;
            core::mem::take(&mut control.subscriptions)
        });
        let ready = self.queue.state.with(|state| {
            state.closed = true;
            self.queue.update(true);
            // Closing excludes both notify and Delivery rollback. Detach once
            // rather than searching the ready queue for every subscription.
            core::mem::take(&mut state.ready)
        });
        drop(ready);
        for entry in entries {
            entry.registration.active.store(false, Ordering::Relaxed);
            let observer = entry
                .source
                .source()
                .state()
                .unsubscribe(entry.registration.id);
            drop(observer);
            drop(entry);
        }
    }
}

impl private::Sealed for WaitSet {}
impl private::UserExportable for WaitSet {}
impl KernelObject for WaitSet {
    const KIND: ObjectKind = ObjectKind::WAIT_SET;
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::WAIT)
        .union(Rights::BIND_WAIT)
        .union(Rights::INSPECT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(&self.queue.signals, ReadyQueue::READABLE))
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.close();
    }
}

impl Drop for WaitSet {
    fn drop(&mut self) {
        self.close();
    }
}

fn invariant() -> ! {
    crate::boot::fail("WaitSet invariant", ())
}

/// Exercises the copyout transaction without architecture-specific user mappings.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn test_delivery_rollback(domain: &ResourceDomain) -> Result<(), WaitSetError> {
    let set = WaitSet::try_new(1, domain)?;
    let charge = domain
        .reserve(ResourceAmount::ZERO.with(
            ResourceKind::KernelMemoryBytes,
            (FallibleArc::<Registration>::allocation_size()
                + core::mem::size_of::<PersistentObserver>()) as u64,
        ))
        .map_err(WaitSetError::Resource)?
        .commit();
    let registration = FallibleArc::try_new(Registration {
        id: 1,
        queue: set.queue.downgrade(),
        active: AtomicBool::new(true),
        pending: AtomicBool::new(false),
        _charge: charge,
    })
    .map_err(|_| WaitSetError::Allocation)?;
    let source = SignalState::new();
    let mask = SignalMask::from_trusted_bits(1);
    source.subscribe(
        try_box(PersistentObserver::new(registration.clone(), mask))
            .map_err(|_| WaitSetError::Allocation)?,
    );
    source
        .update(SignalMask::EMPTY, mask)
        .map_err(|_| WaitSetError::InvalidInput)?;
    let first = set.wait(0, domain, || false)?;
    let expected = first.record();
    drop(first); // A failed userspace copy must preserve the notification.
    if !registration.pending() || source.rearm_subscription(1) {
        return Err(WaitSetError::InvalidInput);
    }
    let second = set.wait(0, domain, || false)?;
    if second.record() != expected {
        return Err(WaitSetError::InvalidInput);
    }
    second.complete();
    if registration.pending() || !source.rearm_subscription(1) {
        return Err(WaitSetError::InvalidInput);
    }
    let third = set.wait(0, domain, || false)?;
    registration.active.store(false, Ordering::Relaxed);
    drop(source.unsubscribe(1));
    set.queue.remove(1);
    drop(third); // Explicit cancellation wins over restoration.
    if set.queue.state.with(|state| !state.ready.is_empty()) {
        return Err(WaitSetError::InvalidInput);
    }
    // Close with one delivery in flight and another notification still queued.
    // Keep registrations active to model a notifier already admitted by a source.
    let closing = WaitSet::try_new(2, domain)?;
    let make_registration = |id| -> Result<_, WaitSetError> {
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                FallibleArc::<Registration>::allocation_size() as u64,
            ))
            .map_err(WaitSetError::Resource)?
            .commit();
        FallibleArc::try_new(Registration {
            id,
            queue: closing.queue.downgrade(),
            active: AtomicBool::new(true),
            pending: AtomicBool::new(false),
            _charge: charge,
        })
        .map_err(|_| WaitSetError::Allocation)
    };
    let first = make_registration(2)?;
    let second = make_registration(3)?;
    let snapshot = source.observe(mask).ok_or(WaitSetError::InvalidInput)?;
    Registration::notify(&first, snapshot);
    let delivery = closing.wait(0, domain, || false)?;
    Registration::notify(&second, snapshot);
    closing.close();
    drop(delivery); // Failed copyout after close must not restore the event.
    Registration::notify(&first, snapshot); // Late notification must be ignored.
    closing.close(); // Final destruction may close the same queue again.
    if closing.queue.state.with(|state| !state.ready.is_empty())
        || !matches!(closing.wait(0, domain, || false), Err(WaitSetError::Closed))
    {
        return Err(WaitSetError::InvalidInput);
    }
    Ok(())
}
