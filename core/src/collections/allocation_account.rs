// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Caller-defined storage accounting, independent of filesystem policy.

/// Admission precedes allocation; dropping a charge releases the reservation.
pub trait StorageBudget {
    type Charge;
    type Error;

    fn reserve(&self, bytes: usize) -> Result<Self::Charge, Self::Error>;
}
