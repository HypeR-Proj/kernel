// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Trusted cursor arithmetic for a single-producer shared output ring.

/// A hostile consumer may neither move backwards nor acknowledge unpublished
/// bytes. Invalid observations preserve the last accepted progress.
pub(crate) const fn accept_consumer(produced: u64, accepted: u64, candidate: u64) -> u64 {
    if candidate >= accepted && candidate <= produced {
        candidate
    } else {
        accepted
    }
}

/// Counters never wrap; capacity and addresses are kernel-owned constants.
pub(crate) const fn writable_slot(produced: u64, consumed: u64, capacity: u64) -> Option<usize> {
    if capacity == 0
        || consumed > produced
        || produced == u64::MAX
        || produced - consumed >= capacity
    {
        None
    } else {
        Some((produced % capacity) as usize)
    }
}
