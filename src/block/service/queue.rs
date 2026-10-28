// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability admission and user-copy transactions for asynchronous block queues.

use crate::authority::Rights;
use crate::block::service::TransferError;
use crate::block::{BlockQueue, NativeBlock, QueueError, QueueLimits, Range};
use crate::mm::user_space::UserSlice;
use alloc::vec::Vec;
use hyper::abi::native::*;
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
};
use {crate::object::KernelObject, crate::object::ObjectPublication};
use {crate::process::Process, crate::process::ProcessError};

fn capability(error: impl Into<ProcessError>) -> TransferError {
    TransferError::Capability(error.into())
}

pub(crate) fn create(
    process: &Process,
    receiver: &InvocationReceiver,
    capacity: u32,
    byte_limit: u64,
    inflight_limit: u32,
) -> Result<HandleValue, TransferError> {
    let source = receiver
        .resolve::<NativeBlock>(Rights::NONE)
        .map_err(capability)?;
    if !source.rights().contains(Rights::READ) && !source.rights().contains(Rights::WRITE) {
        return Err(TransferError::Status(HYPER_NATIVE_STATUS_ACCESS_DENIED));
    }
    let limits = QueueLimits {
        capacity: capacity as usize,
        byte_limit: usize::try_from(byte_limit).map_err(|_| queue_error(QueueError::Invalid))?,
        inflight_limit: inflight_limit as usize,
    };
    let reservation = process.reserve_handles::<1>().map_err(capability)?;
    let prepared = (|| {
        let queue = source
            .object()
            .create_queue(process.resource_domain(), limits)
            .map_err(queue_error)?;
        let publication = ObjectPublication::try_new(queue)
            .map_err(|_| TransferError::Status(HYPER_NATIVE_STATUS_NO_MEMORY))?;
        PreparedHandle::try_from_new_object(
            publication,
            source
                .rights()
                .intersection(BlockQueue::SUPPORTED_RIGHTS)
                .union(Rights::DUPLICATE),
            HandleFlags::NONE,
        )
        .map_err(|_| TransferError::Status(HYPER_NATIVE_STATUS_NO_MEMORY))
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            process.abort_handles(reservation);
            return Err(error);
        }
    };
    process
        .publish_handles(reservation, [prepared])
        .map(|handles| handles[0])
        .map_err(|failure| capability(failure.error))
}

/// Copy and validate the complete submission before publishing any disk work.
/// A prepared submission reserves queue/device credits and sponsor charges before
/// payload allocation or copy. Faults roll it back without publishing disk work.
pub(crate) fn submit(
    process: &Process,
    receiver: &InvocationReceiver,
    operation: u32,
    count: u32,
    frame: Option<UserSlice>,
) -> Result<u64, TransferError> {
    let rights = match operation {
        0 => Rights::READ,
        1 | 2 => Rights::WRITE,
        _ => return Err(queue_error(QueueError::Invalid)),
    };
    let queue = receiver.resolve::<BlockQueue>(rights).map_err(capability)?;
    if operation == 2 {
        if count != 0 || frame.is_some() {
            return Err(queue_error(QueueError::Invalid));
        }
        return queue
            .object()
            .prepare(operation, Vec::new())
            .map_err(queue_error)?
            .commit()
            .map_err(queue_error);
    }
    if count == 0 || u64::from(count) > HYPER_NATIVE_BLOCK_QUEUE_BATCH_MAX {
        return Err(queue_error(QueueError::Invalid));
    }
    let frame = frame.ok_or_else(|| queue_error(QueueError::Invalid))?;
    let header_bytes = count as usize * HYPER_NATIVE_BLOCK_QUEUE_RANGE_BYTES as usize;
    if frame.length() < header_bytes as u64 || frame.length() > HYPER_NATIVE_BLOCK_QUEUE_FRAME_BYTES
    {
        return Err(queue_error(QueueError::Invalid));
    }
    let mut header = [0u8; HYPER_NATIVE_BLOCK_QUEUE_BATCH_MAX as usize
        * HYPER_NATIVE_BLOCK_QUEUE_RANGE_BYTES as usize];
    let source = UserSlice::new(frame.base(), header_bytes as u64)
        .map_err(|_| queue_error(QueueError::Invalid))?;
    process
        .copy_from_user(source, &mut header[..header_bytes])
        .map_err(capability)?;
    let mut total = 0usize;
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(count as usize)
        .map_err(|_| queue_error(QueueError::NoMemory))?;
    for record in header[..header_bytes].chunks_exact(16) {
        let first = u64::from_le_bytes(
            record[..8]
                .try_into()
                .map_err(|_| queue_error(QueueError::Invalid))?,
        );
        let bytes = usize::try_from(u64::from_le_bytes(
            record[8..]
                .try_into()
                .map_err(|_| queue_error(QueueError::Invalid))?,
        ))
        .map_err(|_| queue_error(QueueError::Invalid))?;
        if bytes == 0
            || !bytes.is_multiple_of(512)
            || first.checked_add((bytes / 512) as u64).is_none()
        {
            return Err(queue_error(QueueError::Invalid));
        }
        total = total
            .checked_add(bytes)
            .filter(|n| *n <= HYPER_NATIVE_BLOCK_QUEUE_TRANSFER_BYTES as usize)
            .ok_or_else(|| queue_error(QueueError::Invalid))?;
        ranges.push(Range { first, bytes });
    }
    let expected = header_bytes + if operation == 1 { total } else { 0 };
    if frame.length() != expected as u64 {
        return Err(queue_error(QueueError::Invalid));
    }
    let mut prepared = queue
        .object()
        .prepare(operation, ranges)
        .map_err(queue_error)?;
    if operation == 1 {
        let base = frame
            .base()
            .checked_add(header_bytes as u64)
            .ok_or_else(|| queue_error(QueueError::Invalid))?;
        let source =
            UserSlice::new(base, total as u64).map_err(|_| queue_error(QueueError::Invalid))?;
        process
            .copy_from_user(source, prepared.bytes_mut())
            .map_err(capability)?;
    }
    prepared.commit().map_err(queue_error)
}

pub(crate) fn wait(
    receiver: &InvocationReceiver,
    ticket: u64,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<(u64, i64), TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::WAIT)
        .map_err(capability)?;
    let completion = queue
        .object()
        .wait(ticket, deadline, cancelled)
        .map_err(queue_error)?;
    Ok((completion.ticket, completion_status(completion.result)))
}

pub(crate) fn wait_space(
    receiver: &InvocationReceiver,
    bytes: u64,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<(), TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::WAIT)
        .map_err(capability)?;
    let bytes = usize::try_from(bytes).map_err(|_| queue_error(QueueError::Invalid))?;
    queue
        .object()
        .wait_space(bytes, deadline, cancelled)
        .map_err(queue_error)
}

pub(crate) fn reap(
    process: &Process,
    receiver: &InvocationReceiver,
    ticket: u64,
    output: Option<UserSlice>,
) -> Result<(i64, usize), TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::NONE)
        .map_err(capability)?;
    let operation = queue.object().operation(ticket).map_err(queue_error)?;
    receiver
        .require(if operation == 0 {
            Rights::READ
        } else {
            Rights::WRITE
        })
        .map_err(capability)?;
    let claim = queue.object().reap(ticket).map_err(queue_error)?;
    let completion = claim.completion();
    let bytes = claim.bytes();
    if output.map_or(0, |slice| slice.length()) != bytes.len() as u64 {
        return Err(queue_error(QueueError::Invalid));
    }
    if let Some(output) = output {
        process.copy_to_user(output, bytes).map_err(capability)?;
    }
    let result = (completion_status(completion.result), completion.bytes);
    claim.commit();
    Ok(result)
}

pub(crate) fn info(receiver: &InvocationReceiver) -> Result<[u8; 128], TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::INSPECT)
        .map_err(capability)?;
    let info = queue.object().info();
    let fields = [
        info.capacity,
        info.byte_limit,
        info.inflight_limit,
        info.outstanding_requests,
        info.outstanding_bytes,
        info.pending_requests,
        info.inflight_commands,
        info.completed_requests,
        info.submitted_batches,
        info.submitted_ranges,
        info.dispatched_commands,
        info.merged_ranges,
        info.completed_batches,
        info.peak_inflight,
        info.device_inflight_limit,
        0,
    ];
    let mut bytes = [0u8; 128];
    for (chunk, value) in bytes.chunks_exact_mut(8).zip(fields) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    Ok(bytes)
}

pub(crate) fn completion_status(result: Result<(), crate::fs::primitives::block::Error>) -> i64 {
    use crate::fs::primitives::block::Error;
    match result {
        Ok(()) => HYPER_NATIVE_STATUS_OK,
        Err(Error::InvalidRange) => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        Err(Error::ReadOnly) => HYPER_NATIVE_STATUS_READ_ONLY,
        Err(Error::Unsupported) => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        Err(Error::Exhausted) => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        Err(Error::Disconnected) => HYPER_NATIVE_STATUS_PEER_CLOSED,
        Err(Error::Io | Error::Corrupt) => HYPER_NATIVE_STATUS_IO_ERROR,
    }
}

fn queue_error(error: QueueError) -> TransferError {
    let status = match error {
        QueueError::Device(error) => return TransferError::Device(error),
        QueueError::Invalid => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
        QueueError::WouldBlock => HYPER_NATIVE_STATUS_WOULD_BLOCK,
        QueueError::NoMemory => HYPER_NATIVE_STATUS_NO_MEMORY,
        QueueError::ResourceLimit => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        QueueError::TimedOut => HYPER_NATIVE_STATUS_TIMED_OUT,
        QueueError::Cancelled => HYPER_NATIVE_STATUS_CANCELLED,
        QueueError::NotFound => HYPER_NATIVE_STATUS_NOT_FOUND,
        QueueError::Busy => HYPER_NATIVE_STATUS_BUSY,
    };
    TransferError::Status(status)
}

/// Export only read authority to the queue's dedicated Native source storage.
pub(crate) fn create_window(
    process: &Process,
    receiver: &InvocationReceiver,
) -> Result<HandleValue, TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::READ)
        .map_err(capability)?;
    let object = queue.object().create_window().map_err(queue_error)?;
    process
        .create_object(
            object,
            Rights::READ
                .union(Rights::MAP)
                .union(Rights::DUPLICATE)
                .union(Rights::TRANSFER)
                .union(Rights::INSPECT),
        )
        .map_err(capability)
}

/// Source authority comes from an active kernel-issued window generation, never
/// from a user pointer or an arbitrary VMO supplied with the request.
pub(crate) fn submit_window(
    process: &Process,
    receiver: &InvocationReceiver,
    token: u64,
    count: u32,
    frame: UserSlice,
) -> Result<u64, TransferError> {
    let queue = receiver
        .resolve::<BlockQueue>(Rights::WRITE)
        .map_err(capability)?;
    if count == 0
        || count as usize > crate::block::plan::MAX_RANGES
        || frame.length() != u64::from(count) * 24
    {
        return Err(queue_error(QueueError::Invalid));
    }
    let mut records = [0u8; crate::block::plan::MAX_RANGES * 24];
    process
        .copy_from_user(frame, &mut records[..count as usize * 24])
        .map_err(capability)?;
    let mut disk = Vec::new();
    let mut source = Vec::new();
    disk.try_reserve_exact(count as usize)
        .map_err(|_| queue_error(QueueError::NoMemory))?;
    source
        .try_reserve_exact(count as usize)
        .map_err(|_| queue_error(QueueError::NoMemory))?;
    for record in records[..count as usize * 24].chunks_exact(24) {
        let word = |at: usize| -> Result<u64, TransferError> {
            Ok(u64::from_le_bytes(
                record[at..at + 8]
                    .try_into()
                    .map_err(|_| queue_error(QueueError::Invalid))?,
            ))
        };
        let first = word(0)?;
        let offset = usize::try_from(word(8)?).map_err(|_| queue_error(QueueError::Invalid))?;
        let bytes = usize::try_from(word(16)?).map_err(|_| queue_error(QueueError::Invalid))?;
        disk.push(Range { first, bytes });
        source.push((offset, bytes));
    }
    let lease = queue
        .object()
        .window_lease(token, &source)
        .map_err(queue_error)?;
    queue
        .object()
        .prepare_payload(
            1,
            disk,
            Some(crate::block::queue::payload::Payload::Window {
                lease,
                ranges: source,
            }),
        )
        .map_err(queue_error)?
        .commit()
        .map_err(queue_error)
}
