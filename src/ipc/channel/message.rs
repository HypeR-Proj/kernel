// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallibly prepared `ByteChannel` messages with sender-sponsored accounting.

use alloc::boxed::Box;
use alloc::vec::Vec;

use hyper::mm::try_box;

use super::{ByteChannelError, ByteMessageInfo, MessageSequence};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};

pub(super) struct Message {
    sequence: MessageSequence,
    bytes: Vec<u8>,
    pub(super) next: Option<Box<Message>>,
    _charge: CommittedCharge,
}

impl Message {
    pub(super) fn info(&self) -> ByteMessageInfo {
        ByteMessageInfo::new(self.sequence, self.bytes.len())
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn set_sequence(&mut self, sequence: MessageSequence) {
        self.sequence = sequence;
    }

    pub(super) fn take_next(&mut self) -> Option<Box<Self>> {
        self.next.take()
    }

    pub(super) fn replace_next(&mut self, next: Option<Box<Self>>) {
        self.next = next;
    }
}

/// Complete message storage prepared before any Channel lock is acquired.
#[must_use = "publish the prepared message or release its sponsored resources"]
pub(crate) struct PreparedByteMessage {
    message: Option<Box<Message>>,
}

impl PreparedByteMessage {
    /// Allocates sender-sponsored storage before source capabilities are claimed.
    ///
    /// The byte buffer is zeroed so a caller can copy directly from user memory
    /// without first allocating an unaccounted staging buffer.
    pub(crate) fn try_new(
        domain: &ResourceDomain,
        byte_count: usize,
    ) -> Result<Self, ByteChannelError> {
        let charged_byte_count =
            u64::try_from(byte_count).map_err(|_| ByteChannelError::MessageTooLarge)?;
        if charged_byte_count > hyper::abi::native::HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES {
            return Err(ByteChannelError::MessageTooLarge);
        }
        let node_bytes = u64::try_from(core::mem::size_of::<Message>())
            .map_err(|_| ByteChannelError::AllocationSize)?;
        let kernel_bytes = node_bytes
            .checked_add(charged_byte_count)
            .ok_or(ByteChannelError::AllocationSize)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, kernel_bytes)
                    .with(ResourceKind::IpcMessages, 1)
                    .with(ResourceKind::IpcBytes, charged_byte_count),
            )?
            .commit();

        let mut owned = Vec::new();
        owned
            .try_reserve_exact(byte_count)
            .map_err(|_| ByteChannelError::Allocation)?;
        owned.resize(byte_count, 0);
        let message = try_box(Message {
            sequence: MessageSequence::UNASSIGNED,
            bytes: owned,
            next: None,
            _charge: charge,
        })
        .map_err(|_| ByteChannelError::Allocation)?;
        Ok(Self {
            message: Some(message),
        })
    }

    /// Copies an existing byte slice for bare-metal channel contract tests.
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn try_copy_from(
        domain: &ResourceDomain,
        bytes: &[u8],
    ) -> Result<Self, ByteChannelError> {
        let mut prepared = Self::try_new(domain, bytes.len())?;
        prepared.bytes_mut().copy_from_slice(bytes);
        Ok(prepared)
    }

    /// Returns the exclusively owned staging buffer before queue reservation.
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        match self.message.as_deref_mut() {
            Some(message) => &mut message.bytes,
            None => message_invariant(),
        }
    }

    pub(crate) fn info(&self) -> ByteMessageInfo {
        self.message().info()
    }

    fn message(&self) -> &Message {
        match self.message.as_deref() {
            Some(message) => message,
            None => message_invariant(),
        }
    }

    pub(super) fn take(mut self) -> Box<Message> {
        match self.message.take() {
            Some(message) => message,
            None => message_invariant(),
        }
    }
}

impl Drop for PreparedByteMessage {
    fn drop(&mut self) {
        // A prepared message remains ordinary local ownership. Dropping it is
        // the allocation-free transaction abort and releases its charge.
        release_messages(self.message.take());
    }
}

/// Iteratively releases a detached queue without retaining object locks or
/// recursively destroying the queue's linked-list spine.
pub(super) fn release_messages(mut current: Option<Box<Message>>) {
    release_messages_inner(current.take());
}

/// Releases a detached queue without recursively dropping its linked-list spine.
pub(super) fn release_messages_into(mut current: Option<Box<Message>>) {
    release_messages_inner(current.take());
}

fn release_messages_inner(mut current: Option<Box<Message>>) {
    while let Some(mut message) = current {
        current = message.take_next();
        drop(message);
    }
}

impl From<ResourceError> for ByteChannelError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

#[cold]
fn message_invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: ByteChannel message invariant failed"))
}
