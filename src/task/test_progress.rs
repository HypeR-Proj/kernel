// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler-aware progress waits for bare-metal integration tests.
//!
//! A count of local yields is not a time bound and does not guarantee that a
//! remote physical CPU runs, especially under TCG. Test observers use this
//! helper to retain a real monotonic deadline while blocking between samples.

use hyper::hal::timer::deadline_reached;

use super::SleepError;

pub(crate) const DEFAULT_TIMEOUT_NS: u64 = 4_000_000_000;

/// Waits until `condition` succeeds or the monotonic deadline expires.
///
/// The callback may observe state or attempt a bounded nonblocking claim; it
/// must not wait itself. The one-millisecond blocking interval is the progress
/// opportunity for the actual owner. `Ok(false)` is an ordinary test timeout,
/// while callback, timer, and scheduling failures retain their typed error
/// through `E`. Kernel timekeeping and the calling Thread's sleep context must
/// already be active.
pub(crate) fn wait_until<E>(
    timeout_nanoseconds: u64,
    mut condition: impl FnMut() -> Result<bool, E>,
) -> Result<bool, E>
where
    E: From<SleepError>,
{
    if condition()? {
        return Ok(true);
    }
    let deadline = crate::time::deadline_after(timeout_nanoseconds)
        .map_err(SleepError::from)
        .map_err(E::from)?;
    loop {
        super::sleep_ms(1).map_err(E::from)?;
        if condition()? {
            return Ok(true);
        }
        if deadline_reached(crate::time::monotonic_ticks(), deadline) {
            return Ok(false);
        }
    }
}
