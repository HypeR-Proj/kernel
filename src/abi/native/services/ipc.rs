// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Byte and capability channel service contracts.

use crate::capability::{HandleValue, InvocationReceiver};
use crate::ipc::{
    ByteChannelReadOutcome, ByteChannelServiceError, CapabilityChannelServiceError,
    CapabilityReceiveOutcome,
};
use crate::mm::user_space::UserSlice;

pub(crate) trait IpcServices {
    fn trace_byte_channel_rpc(
        &self,
        endpoint: &InvocationReceiver,
        arguments: &[u64; 6],
    ) -> Result<[u64; 2], hyper::abi::native::HyperNativeStatus>;

    fn create_byte_channel(&self) -> Result<[HandleValue; 2], ByteChannelServiceError>;
    fn create_capability_channel(&self) -> Result<[HandleValue; 2], CapabilityChannelServiceError>;
    fn write_byte_channel(
        &self,
        endpoint: &InvocationReceiver,
        bytes: Option<UserSlice>,
    ) -> Result<(), ByteChannelServiceError>;
    fn read_byte_channel(
        &self,
        endpoint: &InvocationReceiver,
        bytes: Option<UserSlice>,
    ) -> Result<ByteChannelReadOutcome, ByteChannelServiceError>;
    fn try_send_capability_channel(
        &self,
        endpoint: &InvocationReceiver,
        bytes: Option<UserSlice>,
        dispositions: Option<UserSlice>,
    ) -> Result<(), CapabilityChannelServiceError>;
    fn receive_capability_channel(
        &self,
        endpoint: &InvocationReceiver,
        deadline: u64,
        bytes: Option<UserSlice>,
        slots: Option<UserSlice>,
    ) -> Result<CapabilityReceiveOutcome, CapabilityChannelServiceError>;
}
