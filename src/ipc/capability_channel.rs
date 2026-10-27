// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Synchronous capability rendezvous with no capability-bearing queue.
//!
//! A receiver registration is a heap-stable allocation owned by its caller.
//! The pair stores only a weak reference in a bounded FIFO. A sender removes
//! one registration before it may claim source handles, so `WouldBlock` never
//! changes source authority and the endpoint graph never owns an in-transit
//! capability batch.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hyper::mm::{FallibleArc, WeakFallibleArc, try_box};
use hyper::sync::InterruptSpinLock;

use crate::authority::Rights;
use crate::sync::Completion;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::ObjectWaitError, crate::object::SignalMask, crate::object::SignalSource,
    crate::object::SignalState, crate::object::TimedWaitPreparation, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::prepare_timed_wait,
    crate::object::private,
};
use {crate::task::WaitOutcome, crate::task::WaitQueue};

use super::service::CapabilityReceiveTarget;

const MAX_RECEIVERS_PER_ENDPOINT: usize = 64;
const MAX_HANDLES: usize = hyper::abi::native::HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES as usize;

type PairLock = InterruptSpinLock<PairState, crate::hal::irq::LocalMask>;
type RegistrationLock = InterruptSpinLock<RegistrationState, crate::hal::irq::LocalMask>;

static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityChannelError {
    Allocation,
    AllocationSize,
    EndpointClosed,
    PeerClosed,
    WouldBlock,
    ReceiverQueueFull,
    BufferTooSmall {
        required_bytes: usize,
        required_handles: usize,
    },
    InvalidHandle,
    AccessDenied,
    WrongObjectType,
    UnsupportedTransfer,
    Busy,
    BadState,
    ResourceLimit,
    UserMemoryFault,
    Internal,
    TimedOut,
    Cancelled,
    Resource(ResourceError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    First,
    Second,
}

impl Side {
    const fn index(self) -> usize {
        self as usize
    }

    const fn peer(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegistrationId(u64);

impl RegistrationId {
    fn allocate() -> Result<Self, CapabilityChannelError> {
        let mut current = NEXT_REGISTRATION_ID.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return Err(CapabilityChannelError::Allocation);
            }
            let next = current
                .checked_add(1)
                .ok_or(CapabilityChannelError::Allocation)?;
            match NEXT_REGISTRATION_ID.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(Self(current)),
                Err(observed) => current = observed,
            }
        }
    }
}

/// Receiver-side constraints for one destination handle slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapabilitySlotContract {
    pub(crate) rights: Rights,
    pub(crate) expected_kind: Option<ObjectKind>,
}

/// Fully validated receive shape published to potential senders.
pub(crate) struct CapabilityReceiveContract {
    byte_capacity: usize,
    slots: Vec<CapabilitySlotContract>,
}

impl CapabilityReceiveContract {
    pub(crate) fn new(
        byte_capacity: usize,
        slots: &[CapabilitySlotContract],
    ) -> Result<Self, CapabilityChannelError> {
        if byte_capacity
            > hyper::abi::native::HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES as usize
            || slots.len() > MAX_HANDLES
        {
            return Err(CapabilityChannelError::BufferTooSmall {
                required_bytes: byte_capacity,
                required_handles: slots.len(),
            });
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(slots.len())
            .map_err(|_| CapabilityChannelError::Allocation)?;
        owned.extend_from_slice(slots);
        Ok(Self {
            byte_capacity,
            slots: owned,
        })
    }

    pub(crate) fn slots(&self) -> &[CapabilitySlotContract] {
        &self.slots
    }

    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    fn accepts(
        &self,
        byte_count: usize,
        handle_count: usize,
    ) -> Result<(), CapabilityChannelError> {
        if byte_count > self.byte_capacity || handle_count > self.slots.len() {
            Err(CapabilityChannelError::BufferTooSmall {
                required_bytes: byte_count,
                required_handles: handle_count,
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapabilityDeliveryInfo {
    pub(crate) bytes: usize,
    pub(crate) handles: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityReceiveOutcome {
    Delivered(CapabilityDeliveryInfo),
    Failed(CapabilityChannelError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationPhase {
    Prepared,
    Queued,
    Matched,
    Completed(CapabilityReceiveOutcome),
}

struct RegistrationState {
    phase: RegistrationPhase,
    park_published: bool,
    target: Option<CapabilityReceiveTarget>,
}

struct ReceiveRegistration {
    id: RegistrationId,
    contract: CapabilityReceiveContract,
    state: RegistrationLock,
    completion: Completion,
    park_queue: WaitQueue,
    _charge: CommittedCharge,
}

impl ReceiveRegistration {
    fn publish_completion(&self) {
        if !self.state.with(|state| state.park_published) {
            return;
        }
        if let Err(error) = self.completion.complete_all() {
            registration_scheduler_invariant("match completion", error);
        }
        if let Err(error) = crate::task::scheduler::wake_one(&self.park_queue) {
            registration_scheduler_invariant("rendezvous wake", error.into());
        }
    }

    fn take_target(&self) -> Option<CapabilityReceiveTarget> {
        self.state.with(|state| state.target.take())
    }
}

struct ReceiverNode {
    id: RegistrationId,
    registration: WeakFallibleArc<ReceiveRegistration>,
    next: Option<Box<ReceiverNode>>,
}

impl hyper::collections::linked_list::NextLink for Box<ReceiverNode> {
    fn next(&self) -> &Option<Self> {
        &self.next
    }
    fn next_mut(&mut self) -> &mut Option<Self> {
        &mut self.next
    }
}

struct ReceiverQueue {
    head: Option<Box<ReceiverNode>>,
    len: usize,
}

impl ReceiverQueue {
    const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    fn push_back(&mut self, node: Box<ReceiverNode>) -> Result<(), Box<ReceiverNode>> {
        if self.len >= MAX_RECEIVERS_PER_ENDPOINT {
            return Err(node);
        }
        hyper::collections::linked_list::insert_before(&mut self.head, node, |_, _| false)?;
        self.len += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<Box<ReceiverNode>> {
        let node = hyper::collections::linked_list::pop_front(&mut self.head)?;
        self.len = self.len.checked_sub(1).unwrap_or_else(queue_invariant);
        Some(node)
    }

    fn remove(&mut self, id: RegistrationId) -> Option<Box<ReceiverNode>> {
        let node =
            hyper::collections::linked_list::remove_first(&mut self.head, |node| node.id == id)?;
        self.len = self.len.checked_sub(1).unwrap_or_else(queue_invariant);
        Some(node)
    }

    fn detach(&mut self) -> Option<Box<ReceiverNode>> {
        self.len = 0;
        self.head.take()
    }

    const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

struct EndpointState {
    open: bool,
    receivers: ReceiverQueue,
    active_matches: usize,
}

impl EndpointState {
    const fn new() -> Self {
        Self {
            open: true,
            receivers: ReceiverQueue::new(),
            active_matches: 0,
        }
    }
}

struct PairState {
    endpoints: [EndpointState; 2],
}

struct CapabilityPair {
    #[cfg(feature = "kernel-trace")]
    trace_id: u64,
    state: PairLock,
    signals: [SignalState; 2],
    publishing: [AtomicBool; 2],
    _charge: CommittedCharge,
}

/// One endpoint of a synchronous capability-rendezvous pair.
pub(crate) struct CapabilityChannel {
    peer_koid: AtomicU64,
    pair: FallibleArc<CapabilityPair>,
    side: Side,
}

impl CapabilityChannel {
    pub(crate) const PEER_RECEIVING: SignalMask = SignalMask::from_trusted_bits(
        hyper::abi::native::HYPER_NATIVE_SIGNAL_CAPABILITY_CHANNEL_PEER_RECEIVING,
    );
    pub(crate) const PEER_CLOSED: SignalMask = SignalMask::from_trusted_bits(
        hyper::abi::native::HYPER_NATIVE_SIGNAL_CAPABILITY_CHANNEL_PEER_CLOSED,
    );
    pub(crate) const SUPPORTED_SIGNALS: SignalMask =
        SignalMask::from_trusted_bits(Self::PEER_RECEIVING.bits() | Self::PEER_CLOSED.bits());

    pub(crate) fn try_pair(
        domain: &ResourceDomain,
    ) -> Result<(Self, Self), CapabilityChannelError> {
        let bytes = object_allocation_size::<Self>()
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(FallibleArc::<CapabilityPair>::allocation_size()))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(CapabilityChannelError::AllocationSize)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, bytes)
                    .with(ResourceKind::KernelObjects, 2),
            )?
            .commit();
        let pair = FallibleArc::try_new(CapabilityPair::new(charge))
            .map_err(|_| CapabilityChannelError::Allocation)?;
        Ok((
            Self {
                peer_koid: AtomicU64::new(0),
                pair: pair.clone(),
                side: Side::First,
            },
            Self {
                peer_koid: AtomicU64::new(0),
                pair,
                side: Side::Second,
            },
        ))
    }

    pub(crate) fn prepare_receive(
        &self,
        domain: &ResourceDomain,
        contract: CapabilityReceiveContract,
    ) -> Result<PreparedCapabilityReceive, CapabilityChannelError> {
        PreparedCapabilityReceive::try_new(self.pair.clone(), self.side, domain, contract)
    }

    pub(crate) fn try_match(
        &self,
        byte_count: usize,
        handle_count: usize,
    ) -> Result<CapabilityReceiveClaim, CapabilityChannelError> {
        CapabilityPair::try_match(&self.pair, self.side, byte_count, handle_count)
    }

    #[cfg(any(test, all(feature = "kernel-test", feature = "kernel-self-test")))]
    pub(crate) fn signal_level_for_test(&self) -> u64 {
        self.pair.signals[self.side.index()]
            .observe(Self::SUPPORTED_SIGNALS)
            .map_or(0, |snapshot| snapshot.signals().bits())
    }

    #[cfg(any(test, all(feature = "kernel-test", feature = "kernel-self-test")))]
    pub(crate) fn close_for_test(&self) {
        self.pair.close(self.side);
    }
}

impl private::Sealed for CapabilityChannel {}
impl private::UserExportable for CapabilityChannel {}

impl KernelObject for CapabilityChannel {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        crate::object::diagnostics::Details::last(
            self.pair.state.with(|state| {
                let local = &state.endpoints[self.side.index()];
                let peer = &state.endpoints[self.side.peer().index()];
                crate::object::diagnostics::DetailRecord::Channel {
                    peer_koid: self.peer_koid.load(Ordering::Acquire),
                    local_open: local.open,
                    peer_open: peer.open,
                    queued: local.receivers.len as u64,
                    bytes: 0,
                    byte_queue: false,
                }
            }),
            cursor,
        )
    }

    const KIND: ObjectKind = ObjectKind::CAPABILITY_CHANNEL;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE);
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.pair.signals[self.side.index()],
            Self::SUPPORTED_SIGNALS,
        ))
    }

    fn on_zero_active_handles(&self, _retirement: &mut ObjectRetirement) {
        self.pair.close(self.side);
    }
}

impl CapabilityPair {
    fn new(charge: CommittedCharge) -> Self {
        #[cfg(feature = "kernel-trace")]
        let trace_id = crate::diagnostics::trace::identity();
        let signals = [SignalState::new(), SignalState::new()];
        #[cfg(feature = "kernel-trace")]
        let [first, second] = signals;
        #[cfg(feature = "kernel-trace")]
        let signals = [
            first.with_trace_identity(trace_id),
            second.with_trace_identity(if trace_id == 0 { 0 } else { trace_id + 1 }),
        ];
        Self {
            #[cfg(feature = "kernel-trace")]
            trace_id,
            state: PairLock::new(PairState {
                endpoints: [EndpointState::new(), EndpointState::new()],
            }),
            signals,
            publishing: [AtomicBool::new(false), AtomicBool::new(false)],
            _charge: charge,
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    fn register(
        &self,
        side: Side,
        registration: &FallibleArc<ReceiveRegistration>,
        node: Box<ReceiverNode>,
    ) -> Result<(), (CapabilityChannelError, Box<ReceiverNode>)> {
        let result = self.state.with(|state| {
            if !state.endpoints[side.index()].open {
                return Err((CapabilityChannelError::EndpointClosed, node));
            }
            if !state.endpoints[side.peer().index()].open {
                return Err((CapabilityChannelError::PeerClosed, node));
            }
            registration.state.with(|state| {
                if state.phase != RegistrationPhase::Prepared {
                    registration_invariant();
                }
                state.phase = RegistrationPhase::Queued;
            });
            state.endpoints[side.index()]
                .receivers
                .push_back(node)
                .map_err(|node| (CapabilityChannelError::ReceiverQueueFull, node))
        });
        if result.is_err() {
            registration.state.with(|state| {
                if state.phase == RegistrationPhase::Queued {
                    state.phase = RegistrationPhase::Prepared;
                }
            });
        }
        self.reconcile_all();
        result
    }

    fn registration_error(&self, side: Side) -> Option<CapabilityChannelError> {
        self.state.with(|state| registration_error(state, side))
    }

    fn try_match(
        pair: &FallibleArc<Self>,
        source: Side,
        byte_count: usize,
        handle_count: usize,
    ) -> Result<CapabilityReceiveClaim, CapabilityChannelError> {
        let target = source.peer();
        let (registration, retired_node) = pair.state.with(|state| {
            if !state.endpoints[source.index()].open {
                return Err(CapabilityChannelError::EndpointClosed);
            }
            if !state.endpoints[target.index()].open {
                return Err(CapabilityChannelError::PeerClosed);
            }
            let node = state.endpoints[target.index()]
                .receivers
                .pop_front()
                .ok_or(CapabilityChannelError::WouldBlock)?;
            let registration = match node.registration.upgrade() {
                Some(registration) => registration,
                // A queued PendingCapabilityReceive is a mandatory strong
                // owner until completion or exact cancellation.
                None => registration_invariant(),
            };
            registration.state.with(|receiver| {
                if receiver.phase != RegistrationPhase::Queued {
                    registration_invariant();
                }
                receiver.phase = RegistrationPhase::Matched;
            });
            state.endpoints[target.index()].active_matches = match state.endpoints[target.index()]
                .active_matches
                .checked_add(1)
            {
                Some(matches) => matches,
                None => pair_invariant(),
            };
            Ok((registration, node))
        })?;
        drop(retired_node);
        pair.reconcile_all();
        let capacity = registration.contract.accepts(byte_count, handle_count);
        Ok(CapabilityReceiveClaim {
            pair: pair.clone(),
            target,
            registration: Some(registration),
            capacity,
        })
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    fn cancel(&self, side: Side, registration: &ReceiveRegistration) -> bool {
        self.cancel_with_error(side, registration, CapabilityChannelError::Cancelled)
    }

    fn cancel_with_error(
        &self,
        side: Side,
        registration: &ReceiveRegistration,
        error: CapabilityChannelError,
    ) -> bool {
        let detached = self.state.with(|state| {
            let node = state.endpoints[side.index()]
                .receivers
                .remove(registration.id)?;
            let target = registration.state.with(|receiver| {
                if receiver.phase != RegistrationPhase::Queued {
                    registration_invariant();
                }
                receiver.phase =
                    RegistrationPhase::Completed(CapabilityReceiveOutcome::Failed(error));
                receiver.target.take()
            });
            Some((node, target))
        });
        if let Some((node, target)) = detached {
            drop(node);
            abort_target(target);
            registration.publish_completion();
            self.reconcile_all();
            true
        } else {
            false
        }
    }

    fn finish_match(
        &self,
        target: Side,
        registration: &ReceiveRegistration,
        outcome: CapabilityReceiveOutcome,
    ) {
        self.state.with(|state| {
            registration.state.with(|receiver| {
                if receiver.phase != RegistrationPhase::Matched {
                    registration_invariant();
                }
                receiver.phase = RegistrationPhase::Completed(outcome);
                #[cfg(feature = "kernel-trace")]
                if let CapabilityReceiveOutcome::Delivered(info) = outcome {
                    crate::tracepoint!(
                        Ipc,
                        "cap_enqueue",
                        queue = self.trace_queue(target),
                        message = registration.id.0,
                        thread = crate::diagnostics::trace::actor(),
                        bytes = info.bytes as u64
                    );
                }
            });
            state.endpoints[target.index()].active_matches = match state.endpoints[target.index()]
                .active_matches
                .checked_sub(1)
            {
                Some(matches) => matches,
                None => pair_invariant(),
            };
        });
        registration.publish_completion();
        self.reconcile_all();
    }

    fn close(&self, side: Side) {
        let (local, peer) = self.state.with(|state| {
            let endpoint = &mut state.endpoints[side.index()];
            if !endpoint.open {
                pair_invariant();
            }
            endpoint.open = false;
            (
                endpoint.receivers.detach(),
                state.endpoints[side.peer().index()].receivers.detach(),
            )
        });
        complete_detached(local, CapabilityChannelError::EndpointClosed);
        complete_detached(peer, CapabilityChannelError::PeerClosed);
        self.reconcile_all();
    }

    #[cfg(feature = "kernel-trace")]
    fn trace_queue(&self, side: Side) -> u64 {
        if self.trace_id == 0 {
            0
        } else {
            self.trace_id + side.index() as u64
        }
    }

    fn reconcile_all(&self) {
        self.reconcile(Side::First);
        self.reconcile(Side::Second);
    }

    fn reconcile(&self, side: Side) {
        let publisher = &self.publishing[side.index()];
        if publisher
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        loop {
            let desired = self.state.with(|state| signal_level(state, side));
            if self.signals[side.index()]
                .update(CapabilityChannel::SUPPORTED_SIGNALS, desired)
                .is_err()
            {
                pair_invariant();
            }
            publisher.store(false, Ordering::Release);
            if self.state.with(|state| signal_level(state, side)) == desired {
                return;
            }
            if publisher
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
        }
    }
}

/// Fallibly allocated receiver state which is not yet visible to a sender.
#[must_use = "publish or discard the capability receive preparation"]
pub(crate) struct PreparedCapabilityReceive {
    pair: FallibleArc<CapabilityPair>,
    side: Side,
    registration: FallibleArc<ReceiveRegistration>,
    node: Option<Box<ReceiverNode>>,
}

impl PreparedCapabilityReceive {
    // Build registration state before the caller reserves user outputs. Its
    // construction frame must not remain live across a COW commit and yield.
    #[inline(never)]
    fn try_new(
        pair: FallibleArc<CapabilityPair>,
        side: Side,
        domain: &ResourceDomain,
        contract: CapabilityReceiveContract,
    ) -> Result<Self, CapabilityChannelError> {
        let bytes = FallibleArc::<ReceiveRegistration>::allocation_size()
            .checked_add(core::mem::size_of::<ReceiverNode>())
            .and_then(|bytes| {
                bytes.checked_add(
                    contract.slots.capacity() * core::mem::size_of::<CapabilitySlotContract>(),
                )
            })
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(CapabilityChannelError::AllocationSize)?;
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes))?
            .commit();
        let registration = FallibleArc::try_new(ReceiveRegistration {
            id: RegistrationId::allocate()?,
            contract,
            state: RegistrationLock::new(RegistrationState {
                phase: RegistrationPhase::Prepared,
                park_published: false,
                target: None,
            }),
            completion: Completion::new(),
            park_queue: WaitQueue::new(),
            _charge: charge,
        })
        .map_err(|_| CapabilityChannelError::Allocation)?;
        let node = try_box(ReceiverNode {
            id: registration.id,
            registration: registration.downgrade(),
            next: None,
        })
        .map_err(|_| CapabilityChannelError::Allocation)?;
        Ok(Self {
            pair,
            side,
            registration,
            node: Some(node),
        })
    }

    /// Registers without parking for deterministic rendezvous contract tests.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn publish(mut self) -> Result<PendingCapabilityReceive, CapabilityChannelError> {
        let node = match self.node.take() {
            Some(node) => node,
            None => registration_invariant(),
        };
        if let Err((error, node)) = self.pair.register(self.side, &self.registration, node) {
            self.node = Some(node);
            return Err(error);
        }
        Ok(PendingCapabilityReceive {
            pair: self.pair.clone(),
            side: self.side,
            registration: self.registration.clone(),
        })
    }

    pub(super) fn attach_target(self, target: CapabilityReceiveTarget) -> Self {
        self.registration.state.with(|state| {
            if state.phase != RegistrationPhase::Prepared || state.target.is_some() {
                registration_invariant();
            }
            state.target = Some(target);
        });
        self
    }

    /// Publishes this receiver only after every timed-park resource exists.
    ///
    /// The pair lock serializes endpoint closure, FIFO visibility, and the
    /// scheduler queue commit. Consequently `PEER_RECEIVING` never describes a
    /// receiver which can still fail to establish its blocking path.
    pub(crate) fn wait(
        mut self,
        domain: &ResourceDomain,
        deadline_nanoseconds: u64,
        cancellation_requested: impl FnOnce() -> bool,
    ) -> Result<CapabilityReceiveOutcome, ObjectWaitError> {
        if let Some(error) = self.pair.registration_error(self.side) {
            return Ok(CapabilityReceiveOutcome::Failed(error));
        }
        let timed = match prepare_timed_wait(domain, deadline_nanoseconds)? {
            TimedWaitPreparation::Completed(WaitOutcome::TimedOut) => {
                return Ok(CapabilityReceiveOutcome::Failed(
                    CapabilityChannelError::TimedOut,
                ));
            }
            TimedWaitPreparation::Completed(outcome) => {
                timed_wait_invariant("unexpected immediate outcome", outcome)
            }
            TimedWaitPreparation::Armed(timed) => timed,
        };
        if cancellation_requested() {
            timed.request_cancellation();
        }
        let mut timed = Some(timed);

        let mut node = self.node.take();
        // SAFETY: the returned mask is consumed by the committed park or
        // dropped before this continuation resumes ordinary execution.
        let (publication, interrupt_mask) = unsafe {
            self.pair.state.with_mask_retained(|state| {
                if let Some(error) = registration_error(state, self.side) {
                    return WaitPublication::Rejected(error);
                }
                let prepared = match timed.take() {
                    Some(prepared) => prepared,
                    None => registration_invariant(),
                };
                let published = prepared.publish_locked(&self.registration.park_queue);
                if !published.will_park() {
                    return WaitPublication::Completed(published);
                }
                self.registration.state.with(|state| {
                    if state.phase != RegistrationPhase::Prepared {
                        registration_invariant();
                    }
                    state.phase = RegistrationPhase::Queued;
                    state.park_published = true;
                });
                let receiver = match node.take() {
                    Some(node) => node,
                    None => registration_invariant(),
                };
                if state.endpoints[self.side.index()]
                    .receivers
                    .push_back(receiver)
                    .is_err()
                {
                    // Capacity was checked under this same lock immediately
                    // before scheduler publication.
                    queue_invariant();
                }
                WaitPublication::Queued(published)
            })
        };

        match publication {
            WaitPublication::Rejected(error) => {
                drop(interrupt_mask);
                let prepared = match timed.take() {
                    Some(prepared) => prepared,
                    None => registration_invariant(),
                };
                let _ = prepared.abort()?;
                Ok(CapabilityReceiveOutcome::Failed(error))
            }
            WaitPublication::Completed(published) => {
                drop(node);
                let (outcome, timer_retirement) = published.finish(interrupt_mask);
                timer_retirement?;
                Ok(interrupted_receive_outcome(outcome))
            }
            WaitPublication::Queued(published) => {
                self.pair.reconcile_all();
                let pending = PendingCapabilityReceive {
                    pair: self.pair.clone(),
                    side: self.side,
                    registration: self.registration.clone(),
                };
                let (outcome, timer_retirement) = published.finish(interrupt_mask);
                let receive = pending.finish_park(outcome);
                timer_retirement?;
                Ok(receive)
            }
        }
    }
}

impl Drop for PreparedCapabilityReceive {
    fn drop(&mut self) {
        let target = self.registration.state.with(|state| {
            if state.phase == RegistrationPhase::Prepared {
                state.target.take()
            } else {
                None
            }
        });
        abort_target(target);
    }
}

enum WaitPublication {
    Rejected(CapabilityChannelError),
    Completed(crate::object::PublishedTimedWait),
    Queued(crate::object::PublishedTimedWait),
}

/// Receiver ownership retained across blocking, timeout, and cancellation.
#[must_use = "observe completion or cancel the capability receive"]
pub(crate) struct PendingCapabilityReceive {
    pair: FallibleArc<CapabilityPair>,
    side: Side,
    registration: FallibleArc<ReceiveRegistration>,
}

impl PendingCapabilityReceive {
    /// Cancels only if no sender has already won the match arbitration.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn cancel(&self) -> bool {
        self.pair.cancel(self.side, &self.registration)
    }

    pub(crate) fn outcome(&self) -> Option<CapabilityReceiveOutcome> {
        self.registration.state.with(|state| match state.phase {
            RegistrationPhase::Completed(outcome) => Some(outcome),
            RegistrationPhase::Prepared
            | RegistrationPhase::Queued
            | RegistrationPhase::Matched => None,
        })
    }

    fn finish_park(self, outcome: WaitOutcome) -> CapabilityReceiveOutcome {
        let result = match outcome {
            WaitOutcome::Notified => self.completed_outcome(),
            WaitOutcome::TimedOut | WaitOutcome::Cancelled => {
                let error = match outcome {
                    WaitOutcome::TimedOut => CapabilityChannelError::TimedOut,
                    WaitOutcome::Cancelled => CapabilityChannelError::Cancelled,
                    WaitOutcome::Notified => registration_invariant(),
                };
                let cancelled = self
                    .pair
                    .cancel_with_error(self.side, &self.registration, error);
                // A matched sender owns the reserved output mappings and
                // handle slots. Repeated process-stop requests may cancel
                // this join too; they cannot retire those resources before
                // the sender commits or rolls back the matched transfer.
                if !cancelled
                    && let Err(join_error) = self.registration.completion.wait_uninterruptible()
                {
                    registration_scheduler_invariant("matched receive join", join_error);
                }
                self.completed_outcome()
            }
        };
        #[cfg(feature = "kernel-trace")]
        if let CapabilityReceiveOutcome::Delivered(info) = result {
            crate::tracepoint!(
                Ipc,
                "cap_dequeue",
                queue = self.pair.trace_queue(self.side),
                message = self.registration.id.0,
                thread = crate::diagnostics::trace::actor(),
                bytes = info.bytes as u64
            );
        }
        result
    }

    fn completed_outcome(&self) -> CapabilityReceiveOutcome {
        match self.outcome() {
            Some(outcome) => outcome,
            None => registration_invariant(),
        }
    }
}

impl Drop for PendingCapabilityReceive {
    fn drop(&mut self) {
        if self.outcome().is_none() {
            registration_invariant();
        }
    }
}

/// Exact receiver selected by FIFO match arbitration.
#[must_use = "complete or reject the matched capability receive"]
pub(crate) struct CapabilityReceiveClaim {
    pair: FallibleArc<CapabilityPair>,
    target: Side,
    registration: Option<FallibleArc<ReceiveRegistration>>,
    capacity: Result<(), CapabilityChannelError>,
}

impl CapabilityReceiveClaim {
    pub(crate) fn contract(&self) -> &CapabilityReceiveContract {
        &self.registration().contract
    }

    pub(crate) const fn capacity_result(&self) -> Result<(), CapabilityChannelError> {
        self.capacity
    }

    pub(super) fn take_target(&self) -> Option<CapabilityReceiveTarget> {
        self.registration().take_target()
    }

    /// Completes the receiver only after the Process direct transfer commits.
    pub(crate) fn commit(mut self, info: CapabilityDeliveryInfo) {
        if self.capacity.is_err() {
            registration_invariant();
        }
        self.finish(CapabilityReceiveOutcome::Delivered(info));
    }

    /// Completes both sides' precommit error path without publishing authority.
    pub(crate) fn reject(mut self, error: CapabilityChannelError) {
        self.finish(CapabilityReceiveOutcome::Failed(error));
    }

    fn registration(&self) -> &ReceiveRegistration {
        match self.registration.as_deref() {
            Some(registration) => registration,
            None => registration_invariant(),
        }
    }

    fn finish(&mut self, outcome: CapabilityReceiveOutcome) {
        let registration = match self.registration.take() {
            Some(registration) => registration,
            None => registration_invariant(),
        };
        self.pair.finish_match(self.target, &registration, outcome);
    }
}

impl Drop for CapabilityReceiveClaim {
    fn drop(&mut self) {
        if self.registration.is_some() {
            registration_invariant();
        }
    }
}

fn complete_detached(mut nodes: Option<Box<ReceiverNode>>, error: CapabilityChannelError) {
    while let Some(mut node) = nodes {
        nodes = node.next.take();
        if let Some(registration) = node.registration.upgrade() {
            let target = registration.state.with(|state| {
                if state.phase != RegistrationPhase::Queued {
                    registration_invariant();
                }
                state.phase = RegistrationPhase::Completed(CapabilityReceiveOutcome::Failed(error));
                state.target.take()
            });
            abort_target(target);
            registration.publish_completion();
        }
    }
}

fn abort_target(target: Option<CapabilityReceiveTarget>) {
    if let Some(target) = target {
        target.abort();
    }
}

fn signal_level(state: &PairState, side: Side) -> SignalMask {
    let endpoint = &state.endpoints[side.index()];
    let peer = &state.endpoints[side.peer().index()];
    let mut level = SignalMask::EMPTY;
    if endpoint.open && peer.open && !peer.receivers.is_empty() {
        level = level.union(CapabilityChannel::PEER_RECEIVING);
    }
    if !peer.open {
        level = level.union(CapabilityChannel::PEER_CLOSED);
    }
    level
}

fn registration_error(state: &PairState, side: Side) -> Option<CapabilityChannelError> {
    if !state.endpoints[side.index()].open {
        Some(CapabilityChannelError::EndpointClosed)
    } else if !state.endpoints[side.peer().index()].open {
        Some(CapabilityChannelError::PeerClosed)
    } else if state.endpoints[side.index()].receivers.len >= MAX_RECEIVERS_PER_ENDPOINT {
        Some(CapabilityChannelError::ReceiverQueueFull)
    } else {
        None
    }
}

fn interrupted_receive_outcome(outcome: WaitOutcome) -> CapabilityReceiveOutcome {
    match outcome {
        WaitOutcome::TimedOut => CapabilityReceiveOutcome::Failed(CapabilityChannelError::TimedOut),
        WaitOutcome::Cancelled => {
            CapabilityReceiveOutcome::Failed(CapabilityChannelError::Cancelled)
        }
        WaitOutcome::Notified => timed_wait_invariant("unpublished wait was notified", outcome),
    }
}

impl From<ResourceError> for CapabilityChannelError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

#[cold]
fn queue_invariant() -> usize {
    registration_invariant()
}

#[cold]
fn pair_invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR CapabilityChannel pair invariant failed"
    ))
}

#[cold]
fn registration_invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR CapabilityChannel registration invariant failed"
    ))
}

#[cold]
fn timed_wait_invariant(message: &str, outcome: WaitOutcome) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR CapabilityChannel timed-wait invariant failed: {message}; outcome={outcome:?}"
    ))
}

#[cold]
fn registration_scheduler_invariant(message: &str, error: crate::sync::Error) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR CapabilityChannel scheduler invariant failed: {message}: {error:?}"
    ))
}

impl crate::object::diagnostics::PairedObject for CapabilityChannel {
    fn bind_peer_identity(&self, peer: crate::object::Koid) {
        // Publication is one-shot; no strong peer reference or ownership cycle.
        self.peer_koid.store(peer.get(), Ordering::Release);
    }
}
