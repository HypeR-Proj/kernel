// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Level-state observation with generation-qualified scheduler arbitration.

use alloc::boxed::Box;

use hyper::mm::{FallibleArc, try_box};
use hyper::sync::InterruptSpinLock;

use crate::accounting::CommittedCharge;
use {crate::task::WaitOutcome, crate::task::WaitQueue, crate::task::WaitTicket};
use {crate::task::scheduler, crate::task::scheduler::WaitRegistration};

type StateLock = InterruptSpinLock<State, crate::hal::irq::LocalMask>;

/// Object-specific signal bits after validation against the object's mask.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SignalMask(u64);

impl SignalMask {
    pub(crate) const EMPTY: Self = Self(0);

    pub(crate) const fn from_bits(bits: u64, supported: Self) -> Option<Self> {
        if bits & !supported.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Constructs bits whose value came from the generated ABI schema.
    pub(crate) const fn from_trusted_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub(crate) const fn bits(self) -> u64 {
        self.0
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// Borrowed type-erased access to one object's level-state signals.
///
/// The object retains the stable `SignalState`; this value carries the exact
/// object-specific mask so generic wait code can validate untrusted bits
/// without dispatching on `ObjectKind`.
#[derive(Clone, Copy)]
pub(crate) struct SignalSource<'object> {
    state: &'object SignalState,
    supported: SignalMask,
}

impl<'object> SignalSource<'object> {
    pub(crate) const fn new(state: &'object SignalState, supported: SignalMask) -> Self {
        Self { state, supported }
    }

    pub(super) fn validate(self, raw: u64, allow_empty: bool) -> Option<SignalMask> {
        let signals = SignalMask::from_bits(raw, self.supported)?;
        if !allow_empty && signals.is_empty() {
            return None;
        }
        Some(signals)
    }

    pub(super) const fn state(self) -> &'object SignalState {
        self.state
    }

    pub(super) const fn has_empty_mask(self) -> bool {
        self.supported.is_empty()
    }

    pub(super) fn same_state(self, other: Self) -> bool {
        core::ptr::eq(self.state, other.state)
    }
}

/// Signal value committed by the exact winning notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SignalSnapshot {
    signals: SignalMask,
    sequence: u64,
}

impl SignalSnapshot {
    pub(super) const fn sequence(self) -> u64 {
        self.sequence
    }
    pub(crate) const fn signals(self) -> SignalMask {
        let Self {
            signals,
            sequence: _,
        } = self;
        signals
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalWaitOutcome {
    Observed(SignalSnapshot),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalWaitError {
    Allocation,
    SequenceExhausted,
    Scheduler(scheduler::Error),
}

impl From<scheduler::Error> for SignalWaitError {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

/// Fallibly allocated but unpublished wait storage.
///
/// Dropping this value is a local abort. Once consumed by `wait_registered`,
/// the `SignalState` owns the node until the exact wait generation completes.
#[must_use = "prepared signal wait storage must be committed or dropped"]
pub(crate) struct PreparedSignalWait {
    waiter: Box<SignalWaiter>,
}

impl PreparedSignalWait {
    pub(crate) const fn allocation_size() -> usize {
        core::mem::size_of::<SignalWaiter>()
    }

    pub(crate) fn try_new(
        requested: SignalMask,
        charge: CommittedCharge,
    ) -> Result<Self, SignalWaitError> {
        let waiter = try_box(SignalWaiter {
            #[cfg(feature = "kernel-trace")]
            trace_source: 0,
            ticket: None,
            _source: None,
            requested,
            observed: None,
            next: None,
            _charge: charge,
        })
        .map_err(|_| SignalWaitError::Allocation)?;
        Ok(Self { waiter })
    }
}

struct SignalWaiter {
    #[cfg(feature = "kernel-trace")]
    trace_source: u64,
    // `None` exists only while the fallible preparation is unpublished.
    ticket: Option<WaitTicket>,
    _source: Option<crate::task::WaitSourceRegistration>,
    requested: SignalMask,
    observed: Option<SignalSnapshot>,
    next: Option<Box<SignalWaiter>>,
    _charge: CommittedCharge,
}

/// A source-owned persistent observer. Its target holds only a weak queue
/// reference, so the observer cannot keep its owning `WaitSet` alive.
pub(super) struct PersistentObserver {
    registration: FallibleArc<super::wait_set::Registration>,
    requested: SignalMask,
    armed: bool,
    next: Option<Box<PersistentObserver>>,
}

impl PersistentObserver {
    pub(super) fn new(
        registration: FallibleArc<super::wait_set::Registration>,
        requested: SignalMask,
    ) -> Self {
        Self {
            registration,
            requested,
            armed: true,
            next: None,
        }
    }

    fn notify(&mut self, snapshot: SignalSnapshot) {
        if self.armed && snapshot.signals.intersects(self.requested) {
            self.armed = false;
            super::wait_set::Registration::notify(&self.registration, snapshot);
        }
    }
}

struct State {
    level: SignalMask,
    sequence: u64,
    park_queue: WaitQueue,
    registrations: Option<Box<SignalWaiter>>,
    observers: Option<Box<PersistentObserver>>,
}

/// Signal state embedded at a stable address in one kernel object.
pub(crate) struct SignalState {
    state: StateLock,
    #[cfg(feature = "kernel-trace")]
    trace_id: core::sync::atomic::AtomicU64,
}

impl SignalState {
    pub(crate) const fn new() -> Self {
        Self::with_initial_level(SignalMask::EMPTY)
    }

    /// Constructs unpublished signal state with one authoritative initial level.
    pub(crate) const fn with_initial_level(level: SignalMask) -> Self {
        Self {
            #[cfg(feature = "kernel-trace")]
            trace_id: core::sync::atomic::AtomicU64::new(0),
            state: StateLock::new(State {
                level,
                sequence: 0,
                park_queue: WaitQueue::new(),
                registrations: None,
                observers: None,
            }),
        }
    }

    /// Bind a Channel's receive queue before either endpoint is published.
    #[cfg(feature = "kernel-trace")]
    pub(crate) fn with_trace_identity(self, id: u64) -> Self {
        self.trace_id
            .store(id, core::sync::atomic::Ordering::Relaxed);
        self
    }

    #[cfg(feature = "kernel-trace")]
    fn trace_identity(&self) -> u64 {
        use core::sync::atomic::Ordering;
        let existing = self.trace_id.load(Ordering::Relaxed);
        if existing != 0 {
            return existing;
        }
        let id = crate::diagnostics::trace::identity();
        self.trace_id
            .compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed)
            .map_or_else(|winner| winner, |_| id)
    }

    pub(crate) fn update(&self, clear: SignalMask, set: SignalMask) -> Result<(), SignalWaitError> {
        let result = self.state.with(|state| {
            let previous = state.level.0;
            let next_level = SignalMask((previous & !clear.0) | set.0);
            if next_level.0 != previous {
                let next_sequence = state
                    .sequence
                    .checked_add(1)
                    .ok_or(UpdateError::SequenceExhausted)?;
                state.level = next_level;
                state.sequence = next_sequence;
            }
            let snapshot = SignalSnapshot {
                signals: state.level,
                sequence: state.sequence,
            };
            let mut observer = state.observers.as_deref_mut();
            while let Some(entry) = observer {
                entry.notify(snapshot);
                observer = entry.next.as_deref_mut();
            }
            notify_matching(state).map_err(UpdateError::Scheduler)
        });
        match result {
            Ok(()) => Ok(()),
            Err(UpdateError::SequenceExhausted) => Err(SignalWaitError::SequenceExhausted),
            Err(UpdateError::Scheduler(error)) => signal_scheduler_invariant(error),
        }
    }

    pub(super) fn subscribe(&self, mut observer: Box<PersistentObserver>) {
        self.state.with(|state| {
            observer.notify(SignalSnapshot {
                signals: state.level,
                sequence: state.sequence,
            });
            observer.next = state.observers.take();
            state.observers = Some(observer);
        });
    }

    pub(super) fn rearm_subscription(&self, id: u64) -> bool {
        self.state.with(|state| {
            let snapshot = SignalSnapshot {
                signals: state.level,
                sequence: state.sequence,
            };
            let mut current = state.observers.as_deref_mut();
            while let Some(entry) = current {
                if entry.registration.id() == id {
                    if entry.registration.pending() || entry.armed {
                        return false;
                    }
                    entry.armed = true;
                    entry.notify(snapshot);
                    return true;
                }
                current = entry.next.as_deref_mut();
            }
            false
        })
    }

    pub(super) fn unsubscribe(&self, id: u64) -> Option<Box<PersistentObserver>> {
        self.state.with(|state| {
            let mut current = &mut state.observers;
            loop {
                if current
                    .as_ref()
                    .is_some_and(|entry| entry.registration.id() == id)
                {
                    let mut removed = current.take()?;
                    *current = removed.next.take();
                    return Some(removed);
                }
                match current {
                    Some(entry) => current = &mut entry.next,
                    None => return None,
                }
            }
        })
    }

    pub(crate) fn observe(&self, requested: SignalMask) -> Option<SignalSnapshot> {
        self.state.with(|state| {
            (!state.level.intersection(requested).is_empty()).then_some(SignalSnapshot {
                signals: state.level,
                sequence: state.sequence,
            })
        })
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn waiter_count(&self) -> usize {
        self.state.with(|state| {
            let mut count = 0usize;
            let mut current = state.registrations.as_deref();
            while let Some(waiter) = current {
                count = count.saturating_add(1);
                current = waiter.next.as_deref();
            }
            count
        })
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn set_sequence_for_test(&self, sequence: u64) {
        self.state.with(|state| {
            if state.registrations.is_some() {
                signal_invariant()
            }
            state.sequence = sequence;
        });
    }

    pub(super) fn wait_registered(
        &self,
        mut prepared: PreparedSignalWait,
        registration: WaitRegistration,
    ) -> Result<SignalWaitOutcome, SignalWaitError> {
        let ticket = registration.ticket();
        prepared.waiter.ticket = Some(ticket);
        #[cfg(feature = "kernel-trace")]
        {
            prepared.waiter.trace_source = self.trace_identity();
        }
        crate::tracepoint!(
            Ipc,
            "signal_wait",
            source_id = prepared.waiter.trace_source,
            thread = ticket.thread().get(),
            wait = ticket.generation(),
            signals = prepared.waiter.requested.bits()
        );
        prepared.waiter._source = Some(crate::task::WaitSourceRegistration::new(
            ticket,
            crate::task::WaitSource::Signal,
        ));

        // SAFETY: The retained local mask is transferred into a committed
        // park below or dropped before normal execution resumes. The waiter
        // node is linked and the condition is rechecked while this same object
        // lock excludes signal mutation.
        let (park, interrupt_mask) = unsafe {
            self.state.with_mask_retained(|state| {
                link_waiter(state, prepared.waiter);
                if let Err(error) = notify_ticket_if_matching(state, ticket) {
                    // Exact notification has not consumed `registration` on
                    // this error path. Finish it explicitly instead of using
                    // its fail-stop Drop as accidental cleanup.
                    return match scheduler::finish_wait(registration) {
                        Ok(_) => Err(CommitError::Notification(error)),
                        Err(cleanup) => Err(CommitError::Notification(cleanup)),
                    };
                }
                scheduler::prepare_registered_park_locked(&state.park_queue, registration)
                    .map_err(CommitError::Prepare)
            })
        };

        let park = match park {
            Ok(park) => park,
            Err(error) => {
                drop(interrupt_mask);
                let removed = self.state.with(|state| unlink_waiter(state, ticket));
                drop(removed);
                return match error {
                    CommitError::Notification(error) => signal_scheduler_invariant(error),
                    CommitError::Prepare(error) => Err(SignalWaitError::Scheduler(error)),
                };
            }
        };
        let outcome = park.retain_mask(interrupt_mask).complete();
        let waiter = self.state.with(|state| unlink_waiter(state, ticket));
        classify_outcome(outcome, waiter.observed)
    }

    /// Links one member of a multi-object wait to an already armed scheduler
    /// generation and immediately rechecks level state under the same lock.
    pub(super) fn register_shared_wait(
        &self,
        mut prepared: PreparedSignalWait,
        ticket: WaitTicket,
    ) {
        prepared.waiter.ticket = Some(ticket);
        #[cfg(feature = "kernel-trace")]
        {
            prepared.waiter.trace_source = self.trace_identity();
        }
        crate::tracepoint!(
            Ipc,
            "signal_wait",
            source_id = prepared.waiter.trace_source,
            thread = ticket.thread().get(),
            wait = ticket.generation(),
            signals = prepared.waiter.requested.bits()
        );
        prepared.waiter._source = Some(crate::task::WaitSourceRegistration::new(
            ticket,
            crate::task::WaitSource::Signal,
        ));
        self.state.with(|state| {
            link_waiter(state, prepared.waiter);
            if let Err(error) = notify_ticket_if_matching(state, ticket) {
                signal_scheduler_invariant(error)
            }
        });
    }

    /// Parks a shared wait registration after every participating signal state
    /// has published its waiter node.
    pub(super) fn park_shared_wait(
        &self,
        registration: WaitRegistration,
    ) -> Result<WaitOutcome, SignalWaitError> {
        // SAFETY: the retained local mask is consumed by the exact context
        // switch or dropped before this function returns. Holding one member's
        // state lock closes its notification-to-queue edge; every other member
        // has already published a waiter and can resolve the armed generation.
        let (park, interrupt_mask) = unsafe {
            self.state.with_mask_retained(|state| {
                scheduler::prepare_registered_park_locked(&state.park_queue, registration)
            })
        };
        let park = match park {
            Ok(park) => park,
            Err(error) => {
                drop(interrupt_mask);
                return Err(error.into());
            }
        };
        Ok(park.retain_mask(interrupt_mask).complete())
    }

    /// Removes one member of a completed shared wait and returns only the
    /// snapshot whose notification won scheduler arbitration.
    pub(super) fn unregister_shared_wait(&self, ticket: WaitTicket) -> Option<SignalSnapshot> {
        let waiter = self.state.with(|state| unlink_waiter(state, ticket));
        waiter.observed
    }
}

enum CommitError {
    Notification(scheduler::Error),
    Prepare(scheduler::Error),
}

enum UpdateError {
    SequenceExhausted,
    Scheduler(scheduler::Error),
}

fn link_waiter(state: &mut State, mut waiter: Box<SignalWaiter>) {
    let ticket = waiter_ticket(&waiter);
    if find_waiter_mut(&mut state.registrations, ticket).is_some() {
        signal_invariant()
    }
    waiter.next = state.registrations.take();
    state.registrations = Some(waiter);
}

fn unlink_waiter(state: &mut State, ticket: WaitTicket) -> Box<SignalWaiter> {
    let mut link = &mut state.registrations;
    loop {
        let matches = match link.as_ref() {
            Some(waiter) => waiter.ticket == Some(ticket),
            None => signal_invariant(),
        };
        if matches {
            let mut removed = match link.take() {
                Some(waiter) => waiter,
                None => signal_invariant(),
            };
            *link = removed.next.take();
            return removed;
        }
        link = match link.as_mut() {
            Some(waiter) => &mut waiter.next,
            None => signal_invariant(),
        };
    }
}

fn notify_matching(state: &mut State) -> Result<(), scheduler::Error> {
    let level = state.level;
    let sequence = state.sequence;
    let mut current = state.registrations.as_deref_mut();
    while let Some(waiter) = current {
        notify_waiter_if_matching(waiter, level, sequence)?;
        current = waiter.next.as_deref_mut();
    }
    Ok(())
}

fn notify_ticket_if_matching(
    state: &mut State,
    ticket: WaitTicket,
) -> Result<(), scheduler::Error> {
    let level = state.level;
    let sequence = state.sequence;
    let waiter = match find_waiter_mut(&mut state.registrations, ticket) {
        Some(waiter) => waiter,
        None => signal_invariant(),
    };
    notify_waiter_if_matching(waiter, level, sequence)
}

fn notify_waiter_if_matching(
    waiter: &mut SignalWaiter,
    level: SignalMask,
    sequence: u64,
) -> Result<(), scheduler::Error> {
    if waiter.observed.is_some() {
        return Ok(());
    }
    let observed = level.intersection(waiter.requested);
    if observed.is_empty() {
        return Ok(());
    }
    let ticket = waiter_ticket(waiter);
    let snapshot = SignalSnapshot {
        // The requested intersection determines readiness, while the winner
        // observes the object's complete supported level at that instant.
        signals: level,
        sequence,
    };
    let resolution = scheduler::notify_registered_with(ticket, || {
        waiter.observed = Some(snapshot);
        crate::tracepoint!(
            Ipc,
            "signal_wake",
            source_id = waiter.trace_source,
            thread = ticket.thread().get(),
            wait = ticket.generation(),
            signals = level.bits()
        );
    })?;
    if resolution.won != waiter.observed.is_some() {
        signal_invariant()
    }
    Ok(())
}

fn find_waiter_mut(
    head: &mut Option<Box<SignalWaiter>>,
    ticket: WaitTicket,
) -> Option<&mut SignalWaiter> {
    let mut current = head.as_deref_mut();
    while let Some(waiter) = current {
        if waiter.ticket == Some(ticket) {
            return Some(waiter);
        }
        current = waiter.next.as_deref_mut();
    }
    None
}

fn waiter_ticket(waiter: &SignalWaiter) -> WaitTicket {
    match waiter.ticket {
        Some(ticket) => ticket,
        None => signal_invariant(),
    }
}

fn classify_outcome(
    outcome: WaitOutcome,
    observed: Option<SignalSnapshot>,
) -> Result<SignalWaitOutcome, SignalWaitError> {
    match (outcome, observed) {
        (WaitOutcome::Notified, Some(snapshot)) => Ok(SignalWaitOutcome::Observed(snapshot)),
        (WaitOutcome::TimedOut, None) => Ok(SignalWaitOutcome::TimedOut),
        (WaitOutcome::Cancelled, None) => Ok(SignalWaitOutcome::Cancelled),
        _ => signal_invariant(),
    }
}

#[cold]
fn signal_invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: object signal invariant failed"))
}

#[cold]
fn signal_scheduler_invariant(error: scheduler::Error) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: object signal scheduler invariant failed: {error:?}"
    ))
}
