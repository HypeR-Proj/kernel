// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Byte and capability channel arguments and record bounds.

use super::optional_user_slice;
use crate::abi::native::Arguments;
use crate::abi::native::status::status_from_address_error;
use crate::mm::user_space::{UserAddress, UserSlice};
use hyper::abi::native::{
    HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES, HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES,
    HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES, HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS,
    HYPER_NATIVE_STATUS_INVALID_ARGUMENT, HyperNativeCapabilityDisposition,
    HyperNativeCapabilityReceiveSlot, HyperNativeObjectWaitItem, HyperNativeStatus,
};

pub(in crate::abi::native) fn parse_wait_many(
    arguments: &Arguments,
) -> Result<(UserSlice, usize, u64), HyperNativeStatus> {
    if arguments[1] == 0 || arguments[1] > HYPER_NATIVE_OBJECT_WAIT_MANY_MAX_ITEMS {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let item_count =
        usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let bytes = arguments[1]
        .checked_mul(core::mem::size_of::<HyperNativeObjectWaitItem>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
    let items =
        UserSlice::new(UserAddress::new(arguments[0]), bytes).map_err(status_from_address_error)?;
    Ok((items, item_count, arguments[2]))
}

pub(in crate::abi::native) fn parse_byte_channel_io(
    arguments: &Arguments,
) -> Result<Option<UserSlice>, HyperNativeStatus> {
    if arguments[0] != 0 || arguments[2] > HYPER_NATIVE_BYTE_CHANNEL_MAX_MESSAGE_BYTES {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    optional_user_slice(arguments[1], arguments[2])
}

pub(in crate::abi::native) fn parse_capability_channel_send(
    arguments: &Arguments,
) -> Result<(Option<UserSlice>, Option<UserSlice>), HyperNativeStatus> {
    if arguments[0] != 0
        || arguments[2] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES
        || arguments[4] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let disposition_bytes = capability_disposition_bytes(arguments[4])?;
    Ok((
        optional_user_slice(arguments[1], arguments[2])?,
        optional_user_slice(arguments[3], disposition_bytes)?,
    ))
}

pub(in crate::abi::native) fn parse_capability_channel_receive(
    arguments: &Arguments,
) -> Result<(u64, Option<UserSlice>, Option<UserSlice>), HyperNativeStatus> {
    if arguments[2] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES
        || arguments[4] > HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let slot_bytes = capability_receive_slot_bytes(arguments[4])?;
    Ok((
        arguments[0],
        optional_user_slice(arguments[1], arguments[2])?,
        optional_user_slice(arguments[3], slot_bytes)?,
    ))
}

pub(in crate::abi::native) fn capability_disposition_bytes(
    record_count: u64,
) -> Result<u64, HyperNativeStatus> {
    capability_record_bytes::<HyperNativeCapabilityDisposition>(record_count)
}

pub(in crate::abi::native) fn capability_receive_slot_bytes(
    record_count: u64,
) -> Result<u64, HyperNativeStatus> {
    capability_record_bytes::<HyperNativeCapabilityReceiveSlot>(record_count)
}

pub(in crate::abi::native) fn capability_record_bytes<Record>(
    record_count: u64,
) -> Result<u64, HyperNativeStatus> {
    record_count
        .checked_mul(core::mem::size_of::<Record>() as u64)
        .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)
}
