// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure framing admission for bounded Native block write batches.

use hyper::abi::native::{
    HYPER_NATIVE_NATIVE_BLOCK_BATCH_MAX, HYPER_NATIVE_NATIVE_BLOCK_BATCH_RECORD_BYTES,
    HYPER_NATIVE_NATIVE_BLOCK_TRANSFER_BYTES,
};
use {
    crate::fs::primitives::block::Error, crate::fs::primitives::block::WriteRequest,
    crate::fs::primitives::block::validate_writes,
};

pub(super) struct WriteBatch<'a> {
    requests: [WriteRequest<'a>; HYPER_NATIVE_NATIVE_BLOCK_BATCH_MAX as usize],
    count: usize,
}
impl WriteBatch<'_> {
    pub(super) fn requests(&self) -> &[WriteRequest<'_>] {
        &self.requests[..self.count]
    }
}

/// Validate every offset, length and overlap before producing any admissible
/// request. Returned slices borrow the syscall's private copy, never user memory.
pub(super) fn decode_write_batch(
    frame: &[u8],
    count: usize,
    sectors: u64,
) -> Result<WriteBatch<'_>, Error> {
    if count == 0 || count > HYPER_NATIVE_NATIVE_BLOCK_BATCH_MAX as usize {
        return Err(Error::InvalidRange);
    }
    let record_bytes = HYPER_NATIVE_NATIVE_BLOCK_BATCH_RECORD_BYTES as usize;
    let header = count.checked_mul(record_bytes).ok_or(Error::InvalidRange)?;
    let mut payload = frame.get(header..).ok_or(Error::InvalidRange)?;
    if payload.len() > HYPER_NATIVE_NATIVE_BLOCK_TRANSFER_BYTES as usize {
        return Err(Error::InvalidRange);
    }
    let mut result = WriteBatch {
        requests: [WriteRequest {
            first: 0,
            bytes: &[],
        }; HYPER_NATIVE_NATIVE_BLOCK_BATCH_MAX as usize],
        count,
    };
    for (index, request) in result.requests[..count].iter_mut().enumerate() {
        let at = index * record_bytes;
        let first = u64::from_le_bytes(
            frame[at..at + 8]
                .try_into()
                .map_err(|_| Error::InvalidRange)?,
        );
        let length = usize::try_from(u64::from_le_bytes(
            frame[at + 8..at + 16]
                .try_into()
                .map_err(|_| Error::InvalidRange)?,
        ))
        .map_err(|_| Error::InvalidRange)?;
        let bytes = payload.get(..length).ok_or(Error::InvalidRange)?;
        payload = &payload[length..];
        *request = WriteRequest { first, bytes };
    }
    if !payload.is_empty() {
        return Err(Error::InvalidRange);
    }
    validate_writes(sectors, result.requests())?;
    Ok(result)
}
