// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded arithmetic and backend-result contracts for file reads.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    ArithmeticOverflow,
    InvalidBackendResult,
}

/// Failure before any bytes have been delivered, or a violated backend contract.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BatchError<E> {
    Contract(Error),
    Transfer(E),
}

/// Drives a bounded read without turning each internal batch into a syscall.
/// The callback returns only bytes already copied to the user's destination.
/// After progress, a transfer failure returns that prefix for a later retry.
pub(crate) fn read_batches<E>(
    offset: u64,
    capacity: usize,
    batch_size: usize,
    mut transfer: impl FnMut(u64, usize, usize) -> Result<usize, E>,
) -> Result<usize, BatchError<E>> {
    if batch_size == 0 {
        return Err(BatchError::Contract(Error::InvalidBackendResult));
    }
    let length =
        u64::try_from(capacity).map_err(|_| BatchError::Contract(Error::ArithmeticOverflow))?;
    offset
        .checked_add(length)
        .ok_or(BatchError::Contract(Error::ArithmeticOverflow))?;
    let mut completed = 0usize;
    while completed < capacity {
        let length = (capacity - completed).min(batch_size);
        let actual = match transfer(offset + completed as u64, completed, length) {
            Ok(actual) => actual,
            Err(_) if completed != 0 => return Ok(completed),
            Err(error) => return Err(BatchError::Transfer(error)),
        };
        if actual > length {
            return Err(BatchError::Contract(Error::InvalidBackendResult));
        }
        completed += actual;
        if actual < length {
            break;
        }
    }
    Ok(completed)
}

/// Visit one contiguous destination subrange without concatenating payloads or
/// allocating pointer metadata. Each callback receives its source-buffer offset
/// and an exclusive destination slice; all bounds are checked before any copy.
pub(crate) fn scatter_copy<E>(
    output: &mut [&mut [u8]],
    mut skip: usize,
    length: usize,
    mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), E>,
) -> Result<(), BatchError<E>> {
    let total = output
        .iter()
        .try_fold(0usize, |total, part| total.checked_add(part.len()))
        .ok_or(BatchError::Contract(Error::ArithmeticOverflow))?;
    if skip.checked_add(length).is_none_or(|end| end > total) {
        return Err(BatchError::Contract(Error::InvalidBackendResult));
    }
    let mut copied = 0;
    for part in output {
        if copied == length {
            break;
        }
        if skip >= part.len() {
            skip -= part.len();
            continue;
        }
        let count = (length - copied).min(part.len() - skip);
        copy(copied, &mut part[skip..skip + count]).map_err(BatchError::Transfer)?;
        copied += count;
        skip = 0;
    }
    Ok(())
}
