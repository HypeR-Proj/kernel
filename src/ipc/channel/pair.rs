// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Ordered endpoint queues and signal reconciliation for one Channel pair.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;

use super::message::{Message, release_messages, release_messages_into};
use super::{
    ByteChannelError, ByteMessageInfo, ByteReceiveClaim, ByteWriteReservation, MessageSequence,
    PreparedByteMessage, ReceivedByteMessage, Side,
};
use crate::accounting::CommittedCharge;
use {crate::object::ObjectRetirement, crate::object::SignalMask, crate::object::SignalState};

type PairLock = InterruptSpinLock<PairState, crate::hal::irq::LocalMask>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum SideLifecycle {
    Open,
    Closing,
    Closed,
}

struct MessageQueue {
    head: Option<Box<Message>>,
    messages: u64,
    bytes: u64,
}

impl MessageQueue {
    const fn new() -> Self {
        Self {
            head: None,
            messages: 0,
            bytes: 0,
        }
    }

    fn push_back(&mut self, message: Box<Message>) {
        let mut link = &mut self.head;
        while let Some(current) = link {
            link = &mut current.next;
        }
        *link = Some(message);
    }

    fn pop_front(&mut self) -> Option<Box<Message>> {
        let mut head = self.head.take()?;
        self.head = head.take_next();
        Some(head)
    }

    fn restore_front(&mut self, mut message: Box<Message>) {
        message.replace_next(self.head.take());
        self.head = Some(message);
    }

    fn detach(&mut self) -> Option<Box<Message>> {
        self.messages = 0;
        self.bytes = 0;
        self.head.take()
    }
}

struct EndpointState {
    lifecycle: SideLifecycle,
    incoming: MessageQueue,
    incoming_reservations: u64,
    reserved_bytes: u64,
    receive_claimed: bool,
}

impl EndpointState {
    const fn new() -> Self {
        Self {
            lifecycle: SideLifecycle::Open,
            incoming: MessageQueue::new(),
            incoming_reservations: 0,
            reserved_bytes: 0,
            receive_claimed: false,
        }
    }
}

struct PairState {
    endpoints: [EndpointState; 2],
    next_message_sequence: u64,
}

pub(super) struct Pair {
    #[cfg(feature = "kernel-trace")]
    trace_id: u64,
    state: PairLock,
    signals: [SignalState; 2],
    publishing: [AtomicBool; 2],
    _charge: CommittedCharge,
}

impl Pair {
    pub(super) fn diagnostic(
        &self,
        side: Side,
        peer_koid: u64,
    ) -> crate::object::diagnostics::DetailRecord {
        self.state.with(|state| {
            let local = &state.endpoints[side.index()];
            let peer = &state.endpoints[side.peer().index()];
            crate::object::diagnostics::DetailRecord::Channel {
                peer_koid,
                local_open: local.lifecycle == SideLifecycle::Open,
                peer_open: peer.lifecycle == SideLifecycle::Open,
                queued: local.incoming.messages,
                bytes: local.incoming.bytes,
                byte_queue: true,
            }
        })
    }

    pub(super) fn new(charge: CommittedCharge) -> Self {
        #[cfg(feature = "kernel-trace")]
        let trace_id = crate::diagnostics::trace::identity();
        let signals = [
            SignalState::with_initial_level(super::ByteChannel::WRITABLE),
            SignalState::with_initial_level(super::ByteChannel::WRITABLE),
        ];
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
                next_message_sequence: 1,
            }),
            signals,
            publishing: [AtomicBool::new(false), AtomicBool::new(false)],
            _charge: charge,
        }
    }

    #[cfg(feature = "kernel-trace")]
    pub(super) fn trace_queue(&self, side: Side) -> u64 {
        if self.trace_id == 0 {
            0
        } else {
            self.trace_id + side.index() as u64
        }
    }

    pub(super) fn signal_state(&self, side: Side) -> &SignalState {
        &self.signals[side.index()]
    }

    pub(super) fn prepare_write(
        pair: &FallibleArc<Self>,
        side: Side,
        message: &PreparedByteMessage,
    ) -> Result<ByteWriteReservation, ByteChannelError> {
        let info = message.info();
        let sequence = pair.state.with(|state| {
            let source = &state.endpoints[side.index()];
            let target = &state.endpoints[side.peer().index()];
            if source.lifecycle != SideLifecycle::Open {
                return Err(ByteChannelError::EndpointClosed);
            }
            if target.lifecycle != SideLifecycle::Open {
                return Err(ByteChannelError::PeerClosed);
            }
            ensure_capacity(target, info)?;
            let sequence = MessageSequence::new(state.next_message_sequence)
                .ok_or(ByteChannelError::SequenceExhausted)?;
            state.next_message_sequence = state
                .next_message_sequence
                .checked_add(1)
                .ok_or(ByteChannelError::SequenceExhausted)?;
            let target = &mut state.endpoints[side.peer().index()];
            target.incoming_reservations = checked_add(target.incoming_reservations, 1);
            target.reserved_bytes = checked_add(target.reserved_bytes, info.bytes_u64());
            Ok(sequence)
        })?;
        pair.reconcile(side);
        Ok(ByteWriteReservation::new(
            pair.clone(),
            side.peer(),
            sequence,
            info,
        ))
    }

    pub(super) fn publish_write(
        &self,
        target: Side,
        sequence: MessageSequence,
        expected: ByteMessageInfo,
        prepared: PreparedByteMessage,
    ) {
        let mut message = prepared.take();
        if message.info().size() != expected.size() {
            pair_invariant("write reservation message changed");
        }
        message.set_sequence(sequence);
        let detached = self.state.with(|state| {
            let endpoint = &mut state.endpoints[target.index()];
            release_reservation(endpoint, expected);
            endpoint.incoming.messages = checked_add(endpoint.incoming.messages, 1);
            endpoint.incoming.bytes = checked_add(endpoint.incoming.bytes, expected.bytes_u64());
            endpoint.incoming.push_back(message);
            // Observe publication under the queue lock, before another CPU can
            // commit a receive. Reservations and failed sends emit no enqueue.
            crate::tracepoint!(
                Ipc,
                "ipc_enqueue",
                queue = self.trace_queue(target),
                message = sequence.0.get(),
                thread = crate::diagnostics::trace::actor(),
                bytes = expected.bytes_u64()
            );
            finalize_close(endpoint)
        });
        self.reconcile_all();
        release_messages(detached);
    }

    pub(super) fn peek(&self, side: Side) -> Result<ByteMessageInfo, ByteChannelError> {
        self.state.with(|state| {
            let endpoint = &state.endpoints[side.index()];
            if endpoint.lifecycle == SideLifecycle::Closed {
                return Err(ByteChannelError::EndpointClosed);
            }
            if endpoint.receive_claimed {
                return Err(ByteChannelError::Busy);
            }
            if let Some(message) = endpoint.incoming.head.as_deref() {
                return Ok(message.info());
            }
            if state.endpoints[side.peer().index()].lifecycle == SideLifecycle::Closed {
                Err(ByteChannelError::PeerClosed)
            } else {
                Err(ByteChannelError::WouldBlock)
            }
        })
    }

    pub(super) fn claim(
        pair: &FallibleArc<Self>,
        side: Side,
        expected: ByteMessageInfo,
    ) -> Result<ByteReceiveClaim, ByteChannelError> {
        let message = pair.state.with(|state| {
            let endpoint = &mut state.endpoints[side.index()];
            if endpoint.lifecycle == SideLifecycle::Closed {
                return Err(ByteChannelError::EndpointClosed);
            }
            if endpoint.receive_claimed {
                return Err(ByteChannelError::Busy);
            }
            let Some(head) = endpoint.incoming.head.as_deref() else {
                return Err(ByteChannelError::StaleMessage);
            };
            if head.info() != expected {
                return Err(ByteChannelError::StaleMessage);
            }
            endpoint.receive_claimed = true;
            match endpoint.incoming.pop_front() {
                Some(message) => Ok(message),
                None => pair_invariant("peeked Channel head disappeared"),
            }
        })?;
        pair.reconcile(side);
        Ok(ByteReceiveClaim::new(pair.clone(), side, message))
    }

    pub(super) fn commit_receive(&self, side: Side, message: Box<Message>) -> ReceivedByteMessage {
        let info = message.info();
        let detached = self.state.with(|state| {
            let endpoint = &mut state.endpoints[side.index()];
            if !endpoint.receive_claimed {
                pair_invariant("Channel receive commit without claim");
            }
            endpoint.receive_claimed = false;
            endpoint.incoming.messages = checked_sub(endpoint.incoming.messages, 1);
            endpoint.incoming.bytes = checked_sub(endpoint.incoming.bytes, info.bytes_u64());
            crate::tracepoint!(
                Ipc,
                "ipc_dequeue",
                queue = self.trace_queue(side),
                message = info.sequence.0.get(),
                thread = crate::diagnostics::trace::actor(),
                bytes = info.bytes_u64()
            );
            finalize_close(endpoint)
        });
        self.reconcile_all();
        release_messages(detached);
        ReceivedByteMessage::new(message)
    }

    pub(super) fn abort_receive(&self, side: Side, message: Box<Message>) {
        let detached = self.state.with(|state| {
            let endpoint = &mut state.endpoints[side.index()];
            if !endpoint.receive_claimed {
                pair_invariant("Channel receive abort without claim");
            }
            endpoint.receive_claimed = false;
            endpoint.incoming.restore_front(message);
            finalize_close(endpoint)
        });
        self.reconcile_all();
        release_messages(detached);
    }

    pub(super) fn close(&self, side: Side, retirement: &mut ObjectRetirement) {
        let detached = self.state.with(|state| {
            let endpoint = &mut state.endpoints[side.index()];
            if endpoint.lifecycle != SideLifecycle::Open {
                pair_invariant("Channel endpoint closed more than once");
            }
            endpoint.lifecycle = SideLifecycle::Closing;
            finalize_close(endpoint)
        });
        self.reconcile_all();
        let _ = retirement;
        release_messages_into(detached);
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
            if let Err(error) =
                self.signals[side.index()].update(super::ByteChannel::SUPPORTED_SIGNALS, desired)
            {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: Channel signal publication failed after commit: {error:?}"
                ));
            }
            publisher.store(false, Ordering::Release);

            let current = self.state.with(|state| signal_level(state, side));
            if current == desired {
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

fn ensure_capacity(
    endpoint: &EndpointState,
    info: ByteMessageInfo,
) -> Result<(), ByteChannelError> {
    if endpoint
        .incoming
        .messages
        .checked_add(endpoint.incoming_reservations)
        .and_then(|used| used.checked_add(1))
        .is_none_or(|used| used > hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_QUEUED_MESSAGES)
        || endpoint
            .incoming
            .bytes
            .checked_add(endpoint.reserved_bytes)
            .and_then(|used| used.checked_add(info.bytes_u64()))
            .is_none_or(|used| {
                used > hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_QUEUED_BYTES
            })
    {
        Err(ByteChannelError::WouldBlock)
    } else {
        Ok(())
    }
}

fn release_reservation(endpoint: &mut EndpointState, info: ByteMessageInfo) {
    endpoint.incoming_reservations = checked_sub(endpoint.incoming_reservations, 1);
    endpoint.reserved_bytes = checked_sub(endpoint.reserved_bytes, info.bytes_u64());
}

fn finalize_close(endpoint: &mut EndpointState) -> Option<Box<Message>> {
    if endpoint.lifecycle == SideLifecycle::Closing
        && endpoint.incoming_reservations == 0
        && !endpoint.receive_claimed
    {
        endpoint.lifecycle = SideLifecycle::Closed;
        endpoint.incoming.detach()
    } else {
        None
    }
}

fn signal_level(state: &PairState, side: Side) -> SignalMask {
    let endpoint = &state.endpoints[side.index()];
    let peer = &state.endpoints[side.peer().index()];
    let mut level = SignalMask::EMPTY;
    if endpoint.incoming.head.is_some() && !endpoint.receive_claimed {
        level = SignalMask::from_trusted_bits(level.bits() | super::ByteChannel::READABLE.bits());
    }
    if endpoint.lifecycle == SideLifecycle::Open
        && peer.lifecycle == SideLifecycle::Open
        && ensure_capacity(peer, ByteMessageInfo::EMPTY).is_ok()
    {
        level = SignalMask::from_trusted_bits(level.bits() | super::ByteChannel::WRITABLE.bits());
    }
    if peer.lifecycle == SideLifecycle::Closed {
        level =
            SignalMask::from_trusted_bits(level.bits() | super::ByteChannel::PEER_CLOSED.bits());
    }
    level
}

fn checked_sub(value: u64, amount: u64) -> u64 {
    match value.checked_sub(amount) {
        Some(result) => result,
        None => pair_invariant("Channel queue accounting underflow"),
    }
}

fn checked_add(value: u64, amount: u64) -> u64 {
    match value.checked_add(amount) {
        Some(result) => result,
        None => pair_invariant("Channel queue accounting overflow"),
    }
}

#[cold]
fn pair_invariant(message: &str) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {message}"))
}
