// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible physical claim preparation owns an armed handle reservation.

pub(super) fn prepare<R, P, E>(
    reservation: R,
    operation: impl FnOnce() -> Result<P, E>,
    abort: impl FnOnce(R),
) -> Result<(R, P), E> {
    match operation() {
        Ok(prepared) => Ok((reservation, prepared)),
        Err(error) => {
            abort(reservation);
            Err(error)
        }
    }
}
