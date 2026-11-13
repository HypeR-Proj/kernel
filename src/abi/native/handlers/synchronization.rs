// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Wire validation for shared synchronization domains.

use crate::abi::native::status::{failure, handle_result, status_only, success};
use crate::abi::native::wire::{
    copy_extensible_input_record, parse_handle, read_record_u64, require_zero,
};
use crate::abi::native::{
    Arguments, NativeAction, SyncWaitRequest, SynchronizationServices, UserMemoryServices,
};
use crate::capability::InvocationReceiver;
use crate::process::sync_wait::{Error, Outcome};
use hyper::abi::native::*;

// These copied records are decoded field-by-field, never borrowed from mutable
// userspace. Keep the decoder geometry coupled to the generated ABI layout.
const _: () = assert!(core::mem::size_of::<HyperNativeSyncDomainBinding>() == 40);
const _: () = assert!(core::mem::size_of::<HyperNativeSyncDomainWait>() == 64);
const _: () = assert!(core::mem::offset_of!(HyperNativeSyncDomainWait, cancellation) == 48);

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_create(
    services: &impl SynchronizationServices,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(handle_result(
        require_zero(arguments).and_then(|()| services.sync_domain_create().map_err(status)),
    ))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_bind(
    services: &(impl SynchronizationServices + UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let record = copy_extensible_input_record::<40>(services, arguments[0], arguments[1], 40)?;
        require_zero(&[read_record_u64(&record, 0), read_record_u64(&record, 32)])?;
        services
            .sync_domain_bind(
                receiver,
                parse_handle(read_record_u64(&record, 8))?,
                read_record_u64(&record, 16),
                read_record_u64(&record, 24),
            )
            .map_err(status)
    })();
    NativeAction::Return(status_only(result))
}

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_join(
    services: &impl SynchronizationServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(
        match require_zero(arguments)
            .and_then(|()| services.sync_domain_join(receiver).map_err(status))
        {
            Ok(token) => success([token, 0]),
            Err(error) => failure(error),
        },
    )
}

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_owner_query(
    services: &impl SynchronizationServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    NativeAction::Return(
        match require_zero(&arguments[1..]).and_then(|()| {
            services
                .sync_domain_owner_query(receiver, arguments[0])
                .map_err(status)
        }) {
            Ok(state) => success([state, 0]),
            Err(error) => failure(error),
        },
    )
}

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_wait_u64(
    services: &(impl SynchronizationServices + UserMemoryServices),
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[2..])?;
        let record = copy_extensible_input_record::<64>(services, arguments[0], arguments[1], 64)?;
        require_zero(&[read_record_u64(&record, 0), read_record_u64(&record, 56)])?;
        let cancellation = match read_record_u64(&record, 48) {
            0 => None,
            handle => Some(parse_handle(handle)?),
        };
        services
            .sync_domain_wait(
                receiver,
                SyncWaitRequest {
                    vmo: parse_handle(read_record_u64(&record, 8))?,
                    offset: read_record_u64(&record, 16),
                    expected: read_record_u64(&record, 24),
                    owner: read_record_u64(&record, 32),
                    deadline: read_record_u64(&record, 40),
                    cancellation,
                },
            )
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok(Outcome::Recheck) => success([HYPER_NATIVE_SYNC_WAIT_RECHECK, 0]),
        Ok(Outcome::OwnerExited(owner)) => success([HYPER_NATIVE_SYNC_WAIT_OWNER_EXITED, owner]),
        Ok(Outcome::TimedOut) => failure(HYPER_NATIVE_STATUS_TIMED_OUT),
        Ok(Outcome::Cancelled) => failure(HYPER_NATIVE_STATUS_CANCELLED),
        Err(error) => failure(error),
    })
}

#[inline(never)]
pub(in crate::abi::native) fn sys_sync_domain_wake_u64(
    services: &impl SynchronizationServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        require_zero(&arguments[3..])?;
        let count =
            u32::try_from(arguments[2]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        services
            .sync_domain_wake(receiver, parse_handle(arguments[0])?, arguments[1], count)
            .map_err(status)
    })();
    NativeAction::Return(match result {
        Ok(woken) => success([woken, 0]),
        Err(error) => failure(error),
    })
}

fn status(error: Error) -> HyperNativeStatus {
    use crate::abi::native::status::*;
    use crate::process::sync_domain::Error as DomainError;
    match error {
        Error::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        Error::Process(error) => status_from_process_error(error),
        Error::Memory(error) => status_from_memory_object_error(error),
        Error::Wait(error) => status_from_object_wait_error(error),
        Error::Domain(error) => match error {
            DomainError::Allocation => HYPER_NATIVE_STATUS_NO_MEMORY,
            DomainError::InvalidInput => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
            DomainError::ConflictingBinding => HYPER_NATIVE_STATUS_BAD_STATE,
            DomainError::AdmissionClosed => HYPER_NATIVE_STATUS_CANCELLED,
            DomainError::Exhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
            DomainError::Resource(error) => status_from_resource_error(error),
        },
    }
}
