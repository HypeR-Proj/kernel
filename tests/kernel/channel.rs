// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal contracts for bounded Channel ordering and level signals.

use crate::kernel::accounting::{ResourceDomain, ResourceLimits};
use crate::kernel::ipc::{ChannelEndpoint, ChannelError, PreparedMessage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Channel(ChannelError),
    Construction,
    StateMismatch(usize),
}

impl From<ChannelError> for Error {
    fn from(error: ChannelError) -> Self {
        Self::Channel(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let (first, second) = ChannelEndpoint::try_pair(&domain)?;

    verify_signals(&first, ChannelEndpoint::WRITABLE.bits(), 1)?;
    verify_signals(&second, ChannelEndpoint::WRITABLE.bits(), 2)?;

    let first_message = PreparedMessage::try_copy_from(&domain, b"first", 0)?;
    first.prepare_write(&first_message)?.publish(first_message);
    verify_signals(
        &second,
        ChannelEndpoint::READABLE.bits() | ChannelEndpoint::WRITABLE.bits(),
        3,
    )?;

    let first_info = second.peek()?;
    let first_claim = second.claim(first_info)?;
    if first_claim.bytes() != b"first" {
        return Err(Error::StateMismatch(4));
    }
    verify_signals(&second, ChannelEndpoint::WRITABLE.bits(), 5)?;
    first_claim.abort();

    let second_message = PreparedMessage::try_copy_from(&domain, b"second", 0)?;
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
    verify_signals(&second, ChannelEndpoint::PEER_CLOSED.bits(), 8)?;
    if second.peek() != Err(ChannelError::PeerClosed) {
        return Err(Error::StateMismatch(9));
    }
    second.close_for_test();
    Ok(())
}

fn verify_signals(endpoint: &ChannelEndpoint, expected: u64, stage: usize) -> Result<(), Error> {
    if endpoint.signal_level_for_test() == expected {
        Ok(())
    } else {
        Err(Error::StateMismatch(stage))
    }
}
