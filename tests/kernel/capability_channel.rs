// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Executed core contracts for synchronous capability rendezvous.

use alloc::vec::Vec;

use crate::kernel::accounting::{ResourceDomain, ResourceLimits};
use crate::kernel::capability::Rights;
use crate::kernel::ipc::{
    CapabilityChannel, CapabilityChannelError, CapabilityDeliveryInfo, CapabilityReceiveContract,
    CapabilityReceiveOutcome, CapabilitySlotContract,
};

const MAX_RECEIVERS: usize = 64;

pub(super) enum Error {
    Channel(CapabilityChannelError),
    Construction,
    State(usize),
    Wait(crate::kernel::object::ObjectWaitError),
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Channel(error) => formatter.debug_tuple("Channel").field(error).finish(),
            Self::Construction => formatter.write_str("Construction"),
            Self::State(stage) => formatter.debug_tuple("State").field(stage).finish(),
            Self::Wait(error) => formatter.debug_tuple("Wait").field(error).finish(),
        }
    }
}

impl From<CapabilityChannelError> for Error {
    fn from(error: CapabilityChannelError) -> Self {
        Self::Channel(error)
    }
}

impl From<crate::kernel::object::ObjectWaitError> for Error {
    fn from(error: crate::kernel::object::ObjectWaitError) -> Self {
        Self::Wait(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain = domain()?;
    verify_nonblocking_and_signal_contract(&domain)?;
    verify_fifo_and_exact_cancellation(&domain)?;
    verify_capacity_rejection(&domain)?;
    verify_timeout_and_cancellation(&domain)?;
    verify_receiver_reservation_bound(&domain)?;
    verify_peer_close(&domain)
}

fn domain() -> Result<ResourceDomain, Error> {
    ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)
}

fn contract(bytes: usize, handles: usize) -> Result<CapabilityReceiveContract, Error> {
    let slots = [CapabilitySlotContract {
        rights: Rights::INSPECT,
        expected_kind: None,
    }; hyper::abi::native::HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES as usize];
    CapabilityReceiveContract::new(bytes, slots.get(..handles).ok_or(Error::Construction)?)
        .map_err(Into::into)
}

fn pair(domain: &ResourceDomain) -> Result<(CapabilityChannel, CapabilityChannel), Error> {
    CapabilityChannel::try_pair(domain).map_err(Into::into)
}

fn verify_nonblocking_and_signal_contract(domain: &ResourceDomain) -> Result<(), Error> {
    let (sender, receiver) = pair(domain)?;
    if !matches!(
        sender.try_match(1, 0),
        Err(CapabilityChannelError::WouldBlock)
    ) {
        return Err(Error::State(1));
    }
    let pending = receiver
        .prepare_receive(domain, contract(16, 1)?)?
        .publish()?;
    if sender.signal_level_for_test() != CapabilityChannel::PEER_RECEIVING.bits() {
        return Err(Error::State(2));
    }
    let claim = sender.try_match(4, 1)?;
    if claim.capacity_result().is_err() || pending.cancel() {
        return Err(Error::State(3));
    }
    claim.commit(CapabilityDeliveryInfo {
        bytes: 4,
        handles: 1,
    });
    if pending.outcome()
        != Some(CapabilityReceiveOutcome::Delivered(
            CapabilityDeliveryInfo {
                bytes: 4,
                handles: 1,
            },
        ))
        || sender.signal_level_for_test() != 0
    {
        return Err(Error::State(4));
    }
    Ok(())
}

fn verify_fifo_and_exact_cancellation(domain: &ResourceDomain) -> Result<(), Error> {
    let (sender, receiver) = pair(domain)?;
    let first = receiver
        .prepare_receive(domain, contract(8, 0)?)?
        .publish()?;
    let second = receiver
        .prepare_receive(domain, contract(8, 0)?)?
        .publish()?;
    let third = receiver
        .prepare_receive(domain, contract(8, 0)?)?
        .publish()?;
    if !second.cancel()
        || second.outcome()
            != Some(CapabilityReceiveOutcome::Failed(
                CapabilityChannelError::Cancelled,
            ))
    {
        return Err(Error::State(5));
    }

    sender.try_match(1, 0)?.commit(CapabilityDeliveryInfo {
        bytes: 1,
        handles: 0,
    });
    if !matches!(
        first.outcome(),
        Some(CapabilityReceiveOutcome::Delivered(_))
    ) || third.outcome().is_some()
    {
        return Err(Error::State(6));
    }
    sender.try_match(2, 0)?.commit(CapabilityDeliveryInfo {
        bytes: 2,
        handles: 0,
    });
    if third.outcome()
        != Some(CapabilityReceiveOutcome::Delivered(
            CapabilityDeliveryInfo {
                bytes: 2,
                handles: 0,
            },
        ))
    {
        return Err(Error::State(7));
    }
    Ok(())
}

fn verify_capacity_rejection(domain: &ResourceDomain) -> Result<(), Error> {
    let (sender, receiver) = pair(domain)?;
    let pending = receiver
        .prepare_receive(domain, contract(2, 0)?)?
        .publish()?;
    let claim = sender.try_match(3, 1)?;
    let error = CapabilityChannelError::BufferTooSmall {
        required_bytes: 3,
        required_handles: 1,
    };
    if claim.capacity_result() != Err(error) {
        return Err(Error::State(8));
    }
    claim.reject(error);
    if pending.outcome() != Some(CapabilityReceiveOutcome::Failed(error)) {
        return Err(Error::State(9));
    }
    Ok(())
}

fn verify_timeout_and_cancellation(domain: &ResourceDomain) -> Result<(), Error> {
    let (_sender, receiver) = pair(domain)?;
    let timeout = receiver
        .prepare_receive(domain, contract(0, 0)?)?
        .wait(domain, 0, || false)?;
    if timeout != CapabilityReceiveOutcome::Failed(CapabilityChannelError::TimedOut) {
        return Err(Error::State(10));
    }
    let cancelled = receiver.prepare_receive(domain, contract(0, 0)?)?.wait(
        domain,
        hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE,
        || true,
    )?;
    if cancelled != CapabilityReceiveOutcome::Failed(CapabilityChannelError::Cancelled) {
        return Err(Error::State(11));
    }
    Ok(())
}

fn verify_receiver_reservation_bound(domain: &ResourceDomain) -> Result<(), Error> {
    let (_sender, receiver) = pair(domain)?;
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(MAX_RECEIVERS)
        .map_err(|_| Error::Construction)?;
    for _ in 0..MAX_RECEIVERS {
        pending.push(
            receiver
                .prepare_receive(domain, contract(0, 0)?)?
                .publish()?,
        );
    }
    if receiver
        .prepare_receive(domain, contract(0, 0)?)?
        .publish()
        .map(|_| ())
        != Err(CapabilityChannelError::ReceiverQueueFull)
    {
        return Err(Error::State(12));
    }
    for registration in pending {
        if !registration.cancel() {
            return Err(Error::State(13));
        }
    }
    Ok(())
}

fn verify_peer_close(domain: &ResourceDomain) -> Result<(), Error> {
    let (sender, receiver) = pair(domain)?;
    let pending = receiver
        .prepare_receive(domain, contract(1, 0)?)?
        .publish()?;
    sender.close_for_test();
    if pending.outcome()
        != Some(CapabilityReceiveOutcome::Failed(
            CapabilityChannelError::PeerClosed,
        ))
        || receiver.signal_level_for_test() != CapabilityChannel::PEER_CLOSED.bits()
    {
        return Err(Error::State(14));
    }
    Ok(())
}
