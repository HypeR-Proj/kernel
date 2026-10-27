// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native ABI boundary for opaque, already-masked handles. Business code passes
//! `HandleValue` unchanged; only register, startup, IPC and diagnostic wire
//! adapters convert integers here. No internal slot or generation is exported.

use core::num::NonZeroU64;

use super::{HANDLE_PREFIX, HANDLE_PREFIX_MASK, HandleError, HandleValue};

/// Checks the wire class, not the table-local payload. All high bytes other
/// than 0x0F, including the zero sentinel, are reserved. Only the owning table
/// can establish whether a token names a live slot with the correct generation.
pub(crate) fn from_abi(value: u64) -> Result<HandleValue, HandleError> {
    if value & HANDLE_PREFIX_MASK != HANDLE_PREFIX {
        return Err(HandleError::InvalidHandle);
    }
    NonZeroU64::new(value)
        .map(HandleValue)
        .ok_or(HandleError::InvalidHandle)
}

/// Serializes exactly the value held by the owning process, including when a
/// different process observes it through an authorized diagnostic capability.
pub(crate) const fn to_abi(value: HandleValue) -> u64 {
    value.0.get()
}
