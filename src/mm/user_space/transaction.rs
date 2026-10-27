// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Retry boundary for unpublished optimistic memory transactions.

/// The operation must have abandoned all unpublished reservations before
/// returning a stale error. A committed result is never retried. The boundary
/// callback checks cancellation and yields without retaining transaction pins.
pub(crate) fn retry_stale<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    stale: impl Fn(&E) -> bool,
    mut retry_boundary: impl FnMut() -> Result<(), E>,
) -> Result<T, E> {
    loop {
        match operation() {
            Err(error) if stale(&error) => retry_boundary()?,
            result => return result,
        }
    }
}
