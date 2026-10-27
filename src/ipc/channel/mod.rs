// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded ordered `ByteChannel` endpoints.

mod message;
mod pair;

use alloc::boxed::Box;
use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};

use hyper::mm::FallibleArc;

use crate::authority::Rights;
use message::Message;
use pair::Pair;
use {
    crate::accounting::ResourceAmount, crate::accounting::ResourceDomain,
    crate::accounting::ResourceError, crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

pub(crate) use message::PreparedByteMessage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ByteChannelError {
    Allocation,
    AllocationSize,
    MessageTooLarge,
    EndpointClosed,
    PeerClosed,
    WouldBlock,
    Busy,
    StaleMessage,
    SequenceExhausted,
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
struct MessageSequence(NonZeroU64);

impl MessageSequence {
    const UNASSIGNED: Self = Self(NonZeroU64::MAX);

    fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value)
            .filter(|sequence| *sequence != NonZeroU64::MAX)
            .map(Self)
    }
}

/// Immutable identity and sizes observed for one current queue head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteMessageInfo {
    sequence: MessageSequence,
    bytes: usize,
}

impl ByteMessageInfo {
    const EMPTY: Self = Self {
        sequence: MessageSequence::UNASSIGNED,
        bytes: 0,
    };

    fn new(sequence: MessageSequence, bytes: usize) -> Self {
        Self { sequence, bytes }
    }

    pub(crate) const fn bytes(self) -> usize {
        self.bytes
    }

    fn bytes_u64(self) -> u64 {
        match u64::try_from(self.bytes) {
            Ok(bytes) => bytes,
            Err(_) => channel_invariant("Channel message byte count exceeded u64"),
        }
    }

    const fn size(self) -> usize {
        self.bytes
    }
}

/// One endpoint of a shared ordered Channel pair.
pub(crate) struct ByteChannel {
    peer_koid: AtomicU64,
    pair: FallibleArc<Pair>,
    side: Side,
}

impl ByteChannel {
    /// Optional observations through authority already held on this endpoint.
    pub(crate) fn trace_rpc(
        &self,
        arguments: &[u64; 6],
    ) -> Result<[u64; 2], hyper::abi::native::HyperNativeStatus> {
        #[cfg(feature = "kernel-trace")]
        {
            crate::diagnostics::trace::annotate_rpc(self.pair.trace_queue(Side::First), arguments)
        }
        #[cfg(not(feature = "kernel-trace"))]
        {
            let _ = arguments;
            Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
        }
    }

    pub(crate) const READABLE: SignalMask = SignalMask::from_trusted_bits(
        hyper::abi::native::HYPER_NATIVE_SIGNAL_BYTE_CHANNEL_READABLE,
    );
    pub(crate) const WRITABLE: SignalMask = SignalMask::from_trusted_bits(
        hyper::abi::native::HYPER_NATIVE_SIGNAL_BYTE_CHANNEL_WRITABLE,
    );
    pub(crate) const PEER_CLOSED: SignalMask = SignalMask::from_trusted_bits(
        hyper::abi::native::HYPER_NATIVE_SIGNAL_BYTE_CHANNEL_PEER_CLOSED,
    );
    pub(crate) const SUPPORTED_SIGNALS: SignalMask = SignalMask::from_trusted_bits(
        Self::READABLE.bits() | Self::WRITABLE.bits() | Self::PEER_CLOSED.bits(),
    );

    pub(crate) fn try_pair(domain: &ResourceDomain) -> Result<(Self, Self), ByteChannelError> {
        let endpoint_bytes = object_allocation_size::<Self>()
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(FallibleArc::<Pair>::allocation_size()))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ByteChannelError::AllocationSize)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, endpoint_bytes)
                    .with(ResourceKind::KernelObjects, 2),
            )?
            .commit();
        let pair =
            FallibleArc::try_new(Pair::new(charge)).map_err(|_| ByteChannelError::Allocation)?;
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

    pub(crate) fn prepare_write(
        &self,
        message: &PreparedByteMessage,
    ) -> Result<ByteWriteReservation, ByteChannelError> {
        Pair::prepare_write(&self.pair, self.side, message)
    }

    pub(crate) fn is_closed(&self) -> bool {
        matches!(
            self.peek(),
            Err(ByteChannelError::EndpointClosed | ByteChannelError::PeerClosed)
        ) || self
            .pair
            .signal_state(self.side)
            .observe(Self::PEER_CLOSED)
            .is_some()
    }

    pub(crate) fn peek(&self) -> Result<ByteMessageInfo, ByteChannelError> {
        self.pair.peek(self.side)
    }

    pub(crate) fn claim(
        &self,
        expected: ByteMessageInfo,
    ) -> Result<ByteReceiveClaim, ByteChannelError> {
        Pair::claim(&self.pair, self.side, expected)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn signal_level_for_test(&self) -> u64 {
        self.pair
            .signal_state(self.side)
            .observe(Self::SUPPORTED_SIGNALS)
            .map_or(0, |snapshot| snapshot.signals().bits())
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn close_for_test(&self) {
        let mut retirement = ObjectRetirement::new();
        self.pair.close(self.side, &mut retirement);
        retirement.drain();
    }
}

impl private::Sealed for ByteChannel {}
impl private::UserExportable for ByteChannel {}

impl KernelObject for ByteChannel {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        crate::object::diagnostics::Details::last(
            self.pair
                .diagnostic(self.side, self.peer_koid.load(Ordering::Acquire)),
            cursor,
        )
    }

    const KIND: ObjectKind = ObjectKind::BYTE_CHANNEL;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE);
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            self.pair.signal_state(self.side),
            Self::SUPPORTED_SIGNALS,
        ))
    }

    fn on_zero_active_handles(&self, retirement: &mut ObjectRetirement) {
        self.pair.close(self.side, retirement);
    }
}

/// Reserved target capacity and sequence for one infallible write commit.
// Allocation and user copies must finish before reserving queue capacity.
#[must_use = "publish the Channel write reservation"]
pub(crate) struct ByteWriteReservation {
    pair: FallibleArc<Pair>,
    target: Side,
    sequence: MessageSequence,
    info: ByteMessageInfo,
    armed: bool,
}

impl ByteWriteReservation {
    fn new(
        pair: FallibleArc<Pair>,
        target: Side,
        sequence: MessageSequence,
        info: ByteMessageInfo,
    ) -> Self {
        Self {
            pair,
            target,
            sequence,
            info,
            armed: true,
        }
    }

    pub(crate) fn publish(mut self, message: PreparedByteMessage) {
        self.armed = false;
        self.pair
            .publish_write(self.target, self.sequence, self.info, message);
    }
}

impl Drop for ByteWriteReservation {
    fn drop(&mut self) {
        if self.armed {
            channel_invariant("unresolved Channel write reservation");
        }
    }
}

/// Exclusive ownership of the exact queue head named by `MessageInfo`.
#[must_use = "commit or abort the Channel receive claim"]
pub(crate) struct ByteReceiveClaim {
    pair: FallibleArc<Pair>,
    side: Side,
    message: Option<Box<Message>>,
}

impl ByteReceiveClaim {
    fn new(pair: FallibleArc<Pair>, side: Side, message: Box<Message>) -> Self {
        Self {
            pair,
            side,
            message: Some(message),
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.message().bytes()
    }

    pub(crate) fn commit(mut self) -> ReceivedByteMessage {
        let message = self.take_message();
        self.pair.commit_receive(self.side, message)
    }

    pub(crate) fn abort(mut self) {
        let message = self.take_message();
        self.pair.abort_receive(self.side, message);
    }

    fn message(&self) -> &Message {
        match self.message.as_deref() {
            Some(message) => message,
            None => channel_invariant("resolved Channel receive claim has no message"),
        }
    }

    fn take_message(&mut self) -> Box<Message> {
        match self.message.take() {
            Some(message) => message,
            None => channel_invariant("Channel receive message consumed twice"),
        }
    }
}

impl Drop for ByteReceiveClaim {
    fn drop(&mut self) {
        if self.message.is_some() {
            channel_invariant("unresolved Channel receive claim");
        }
    }
}

/// Consumed message ownership after queue counters and signals have committed.
pub(crate) struct ReceivedByteMessage {
    message: Option<Box<Message>>,
}

impl ReceivedByteMessage {
    fn new(message: Box<Message>) -> Self {
        Self {
            message: Some(message),
        }
    }

    pub(crate) fn release(mut self) {
        message::release_messages(self.message.take());
    }
}

impl Drop for ReceivedByteMessage {
    fn drop(&mut self) {
        if self.message.is_some() {
            channel_invariant("consumed Channel message was not released");
        }
    }
}

#[cold]
fn channel_invariant(message: &str) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR ByteChannel: {message}"))
}

impl crate::object::diagnostics::PairedObject for ByteChannel {
    fn bind_peer_identity(&self, peer: crate::object::Koid) {
        // Publication is one-shot; no strong peer reference or ownership cycle.
        self.peer_koid.store(peer.get(), Ordering::Release);
    }
}
