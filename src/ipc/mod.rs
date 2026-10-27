// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel IPC objects and message transport policy.

mod capability_channel;
mod capability_wire;
mod channel;
mod service;

pub(crate) use capability_channel::{
    CapabilityChannel, CapabilityChannelError, CapabilityDeliveryInfo, CapabilityReceiveContract,
    CapabilityReceiveOutcome, CapabilitySlotContract,
};
pub(crate) use channel::{ByteChannel, ByteChannelError, PreparedByteMessage};
pub(crate) use service::{
    ByteChannelReadOutcome, ByteChannelServiceError, CapabilityChannelServiceError,
    byte_channel_create, byte_channel_read, byte_channel_write, capability_channel_create,
    capability_channel_receive, capability_channel_try_send,
};
