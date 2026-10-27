// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-local authority over kernel objects.
//!
//! This module owns generation handles, rights enforcement, and the
//! unpublished-slot transaction. It does not own the shared authority
//! vocabulary, object identity, object payload policy, IPC, process locking,
//! user copies, or architecture entry. A Process will serialize its
//! `HandleTable`; resolved object references remain valid after that lock is
//! released, while authority is checked only during resolution.

mod handle;
mod transfer;

pub(crate) use crate::authority::Rights;
pub(crate) use handle::wire as handle_wire;
pub(crate) use handle::{
    ClosedHandle, DirectHandleTransfer, HandleBatchReservation, HandleBatchReservationStorage,
    HandleError, HandleFlags, HandleInfo, HandleReservation, HandleScanCursor, HandleSidecar,
    HandleSidecarPlan, HandleSnapshot, HandleSnapshotPage, HandleTable, HandleTableStoragePlan,
    HandleTableStorageSnapshot, HandleTransferClaim, HandleTransferOperation,
    HandleTransferRequest, HandleTransferRoute, HandleTransferStorage, HandleValue,
    InvocationReceiver, PreparedHandle, ResolvedObject, ResolvedWaitable,
    RetiredDirectHandleTransfer, RetiredHandleBatchReservationStorage,
};
#[cfg(test)]
pub(crate) use handle::{HandleTableLockOrder, InTransitHandleBatch};
pub(crate) use transfer::InTransitCapabilities;

/// Stops after a private capability invariant is violated.
///
/// This leaf path must remain allocation- and ordinary-lock-free so it is
/// safe from handle-table, transfer, and teardown contexts. Diagnostics use
/// the binary panic handler, never the ordinary log path.
#[cold]
#[track_caller]
fn invariant_violation() -> ! {
    hyper::debug::invariant_failure("capability ownership invariant")
}
