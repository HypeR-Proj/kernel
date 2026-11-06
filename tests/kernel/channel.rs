// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal contracts for bounded `ByteChannel` ordering and level signals.

use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {crate::ipc::ByteChannel, crate::ipc::ByteChannelError, crate::ipc::PreparedByteMessage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Channel(ByteChannelError),
    Construction,
    StateMismatch(usize),
}

impl From<ByteChannelError> for Error {
    fn from(error: ByteChannelError) -> Self {
        Self::Channel(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    verify_capacity_and_reservations(&domain)?;
    let (first, second) = ByteChannel::try_pair(&domain)?;

    verify_signals(&first, ByteChannel::WRITABLE.bits(), 1)?;
    verify_signals(&second, ByteChannel::WRITABLE.bits(), 2)?;

    let first_message = PreparedByteMessage::try_copy_from(&domain, b"first")?;
    first.prepare_write(&first_message)?.publish(first_message);
    verify_signals(
        &second,
        ByteChannel::READABLE.bits() | ByteChannel::WRITABLE.bits(),
        3,
    )?;

    let first_info = second.peek()?;
    let first_claim = second.claim(first_info)?;
    if first_claim.bytes() != b"first" {
        return Err(Error::StateMismatch(4));
    }
    verify_signals(&second, ByteChannel::WRITABLE.bits(), 5)?;
    first_claim.abort();

    let second_message = PreparedByteMessage::try_copy_from(&domain, b"second")?;
    first
        .prepare_write(&second_message)?
        .publish(second_message);

    let restored = second.claim(first_info)?;
    if restored.bytes() != b"first" {
        return Err(Error::StateMismatch(6));
    }
    restored.commit().release();
    let next = second.peek()?;
    let second_claim = second.claim(next)?;
    if second_claim.bytes() != b"second" {
        return Err(Error::StateMismatch(7));
    }
    second_claim.commit().release();

    first.close_for_test();
    verify_signals(&second, ByteChannel::PEER_CLOSED.bits(), 8)?;
    if second.peek() != Err(ByteChannelError::PeerClosed) {
        return Err(Error::StateMismatch(9));
    }
    second.close_for_test();
    Ok(())
}

fn verify_signals(endpoint: &ByteChannel, expected: u64, stage: usize) -> Result<(), Error> {
    if endpoint.signal_level_for_test() == expected {
        Ok(())
    } else {
        Err(Error::StateMismatch(stage))
    }
}

fn verify_capacity_and_reservations(domain: &ResourceDomain) -> Result<(), Error> {
    if !ByteChannel::verify_writable_capacity_for_test() {
        return Err(Error::StateMismatch(10));
    }
    let (sender, receiver) = ByteChannel::try_pair(domain)?;
    let size = hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES as usize;
    let slots = hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_QUEUED_MESSAGES;
    let bytes = hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_QUEUED_BYTES;
    let count = slots.min(bytes / size as u64);
    for _ in 0..count {
        let message = PreparedByteMessage::try_new(domain, size)?;
        sender.prepare_write(&message)?.publish(message);
    }
    verify_signals(&sender, 0, 11)?;
    let message = PreparedByteMessage::try_new(domain, size)?;
    if !matches!(
        sender.prepare_write(&message),
        Err(ByteChannelError::WouldBlock)
    ) {
        return Err(Error::StateMismatch(12));
    }
    let claim = receiver.claim(receiver.peek()?)?;
    verify_signals(&sender, 0, 13)?;
    claim.abort();
    verify_signals(&sender, 0, 14)?;
    receiver.claim(receiver.peek()?)?.commit().release();
    verify_signals(&sender, ByteChannel::WRITABLE.bits(), 15)?;
    let reservation = sender.prepare_write(&message)?;
    // Outstanding reservations consume the same credits as queued messages.
    let reserved_level = sender.signal_level_for_test();
    reservation.publish(message);
    if reserved_level != 0 {
        return Err(Error::StateMismatch(16));
    }
    verify_signals(&sender, 0, 17)?;
    for _ in 0..count {
        receiver.claim(receiver.peek()?)?.commit().release();
    }
    verify_signals(&sender, ByteChannel::WRITABLE.bits(), 18)?;
    sender.close_for_test();
    receiver.close_for_test();
    Ok(())
}
