// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Linear Process handle reservation, transfer, and consumption transactions.

use super::handles::require_handle_phase;
use super::{
    HandleAccounting, HandleChargeRecord, Process, ProcessError, ProcessId, ProcessState,
    process_invariant_violation,
};
use hyper::mm::FallibleArc;
use {crate::accounting::ChargeReservation, crate::accounting::CommittedCharge};
use {
    crate::capability::DirectHandleTransfer, crate::capability::HandleBatchReservation,
    crate::capability::HandleReservation, crate::capability::HandleTable,
    crate::capability::HandleTransferClaim, crate::capability::HandleValue,
    crate::capability::InTransitCapabilities, crate::capability::PreparedHandle,
    crate::capability::RetiredDirectHandleTransfer,
};

pub(crate) struct HandlePublishFailure<const N: usize> {
    pub(crate) error: ProcessError,
    #[expect(
        dead_code,
        reason = "Failure retains ownership of unpublished handles until the caller releases it"
    )]
    pub(crate) handles: [PreparedHandle; N],
}

/// Exact typed handle claimed for an object lifecycle operation.
///
/// Preparation is reversible and generation preserving. Once the object
/// operation succeeds, `commit` cannot fail even if Process stop raced after
/// admission; the outstanding table claim prevents teardown from completing.
#[must_use = "commit or roll back the prepared handle consumption"]
pub(crate) struct PreparedHandleConsumption {
    pub(super) transfer: Option<PreparedProcessHandleTransfer>,
}

impl PreparedHandleConsumption {
    pub(crate) fn rollback(mut self) {
        match self.transfer.take() {
            Some(transfer) => transfer.rollback(),
            None => process_invariant_violation(),
        }
    }

    /// Commits the consume-only transaction and releases its object owners.
    ///
    /// Unlike a transfer transaction, successful consumption has no next
    /// namespace. Keeping release inside this linear API prevents callers from
    /// accidentally dropping an unreleased in-transit owner.
    pub(crate) fn commit_and_release(mut self) {
        let transfer = match self.transfer.take() {
            Some(transfer) => transfer,
            None => process_invariant_violation(),
        };
        transfer.commit_pre_admitted().release();
    }

    /// Atomically replaces the claimed source handle with prepared outputs.
    ///
    /// Both the source claim and destination reservation were admitted before
    /// this commit point. Consequently this operation deliberately does not
    /// re-check the Process lifecycle: a racing stop cannot turn a successful
    /// kernel operation into an ABI failure after its source capability was
    /// consumed. The table mutation and its accounting publication share the
    /// Process lock and are allocation-free.
    pub(crate) fn commit_replacement<const N: usize>(
        mut self,
        mut destination: ProcessHandleReservation<N>,
        handles: [PreparedHandle; N],
    ) -> [HandleValue; N] {
        let mut source = match self.transfer.take() {
            Some(source) => source,
            None => process_invariant_violation(),
        };
        let process = source.process.clone();
        destination.require_owner(&process);
        let mut handles = Some(handles);
        let mut detached_source = None;
        let mut retired_transfer_storage = None;
        let mut retired_charge_storage = None;
        let values = process.inner.state.with(|state| {
            for value in source.moved_values.iter().copied() {
                if !state.handle_accounting.is_live(value) {
                    process_invariant_violation();
                }
            }
            let source_claim = match source.claim.take() {
                Some(claim) => claim,
                None => process_invariant_violation(),
            };
            let destination_token = match destination.reservation.take() {
                Some(reservation) => reservation,
                None => process_invariant_violation(),
            };
            let published = match handles.take() {
                Some(handles) => handles,
                None => process_invariant_violation(),
            };
            let ((detached, retired), values) = process.inner.handles.with(|table| {
                let detached = source_claim.commit_with_storage(table);
                let values = destination_token.publish(table, published);
                (detached, values)
            });
            detached_source = Some(detached);
            retired_transfer_storage = Some(retired);

            source.detach_source_charges(&mut state.handle_accounting);

            let mut charges = match destination.handle_charges.take() {
                Some(charges) => charges,
                None => process_invariant_violation(),
            };
            let record = match destination.record.take() {
                Some(record) => record,
                None => process_invariant_violation(),
            };
            state.handle_accounting.install(record, &mut charges);
            retired_charge_storage = Some(charges);
            values
        });

        drop(retired_transfer_storage.take());
        drop(retired_charge_storage.take());
        drop(source.entry_charge.take());
        source.release_scratch();
        let source_storage_charge = match source.handle_charge.take() {
            Some(charge) => charge,
            None => process_invariant_violation(),
        };
        let detached_source = match detached_source.take() {
            Some(handles) => handles,
            None => process_invariant_violation(),
        };
        InTransitCapabilities::new(detached_source, source_storage_charge).release();
        drop(destination.handle_charges.take());
        drop(destination.record.take());
        process.reclaim_handle_pages();
        values
    }
}

impl Drop for PreparedHandleConsumption {
    fn drop(&mut self) {
        if self.transfer.is_some() {
            process_invariant_violation();
        }
    }
}

/// Process-owned reversible source-handle transaction.
#[must_use = "commit or roll back the process handle transfer"]
pub(crate) struct PreparedProcessHandleTransfer {
    pub(super) process: Process,
    pub(super) claim: Option<HandleTransferClaim>,
    pub(super) entry_charge: Option<CommittedCharge>,
    pub(super) handle_charge: Option<CommittedCharge>,
    pub(super) scratch_charge: Option<CommittedCharge>,
    pub(super) released_charges: alloc::vec::Vec<CommittedCharge>,
    pub(super) retired_records: alloc::vec::Vec<FallibleArc<HandleChargeRecord>>,
    pub(super) moved_values: alloc::vec::Vec<HandleValue>,
}

impl PreparedProcessHandleTransfer {
    /// Detaches committed source charges under the Process state lock.
    ///
    /// The source values were validated before table mutation. Both retention
    /// buffers were reserved before locking; retain every owner here so neither
    /// quota release nor record destruction runs while the Process is locked.
    pub(super) fn detach_source_charges(&mut self, accounting: &mut HandleAccounting) {
        for value in self.moved_values.drain(..) {
            let (charge, retired_record) = accounting.release(value);
            self.released_charges.push(charge);
            if let Some(record) = retired_record {
                self.retired_records.push(record);
            }
        }
    }

    // A drained Vec still owns its allocation. Release all three buffers before
    // returning their shared scratch quota, on both rollback and commit paths.
    pub(super) fn release_scratch(&mut self) {
        drop(core::mem::take(&mut self.released_charges));
        drop(core::mem::take(&mut self.retired_records));
        drop(core::mem::take(&mut self.moved_values));
        drop(self.scratch_charge.take());
    }

    pub(crate) fn handle_count(&self) -> usize {
        match self.claim.as_ref() {
            Some(claim) => claim.len(),
            None => process_invariant_violation(),
        }
    }

    /// Restores every claimed source at its original numeric value.
    pub(crate) fn rollback(mut self) {
        let claim = match self.claim.take() {
            Some(claim) => claim,
            None => process_invariant_violation(),
        };
        let retired = self.process.inner.state.with(|_| {
            self.process
                .inner
                .handles
                .with(|table| claim.rollback_with_storage(table))
        });
        drop(retired);
        drop(self.entry_charge.take());
        drop(self.handle_charge.take());
        self.release_scratch();
    }

    /// Permanently consumes all source values and returns their active owners.
    ///
    /// Admission is the only recoverable check. Once it succeeds, accounting
    /// extraction and generation advancement are infallible and serialized by
    /// the Process lock. Released accounting owners are dropped afterward.
    // The recoverable error must retain this complete linear transaction.
    // Boxing it would make rollback depend on a new allocation.
    #[allow(clippy::result_large_err)]
    pub(crate) fn commit(mut self) -> Result<InTransitCapabilities, HandleTransferCommitFailure> {
        let result = self.process.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            for value in self.moved_values.iter().copied() {
                if !state.handle_accounting.is_live(value) {
                    process_invariant_violation();
                }
            }
            for value in self.moved_values.drain(..) {
                let (charge, retired_record) = state.handle_accounting.release(value);
                self.released_charges.push(charge);
                if let Some(record) = retired_record {
                    self.retired_records.push(record);
                }
            }
            let claim = match self.claim.take() {
                Some(claim) => claim,
                None => process_invariant_violation(),
            };
            Ok(self
                .process
                .inner
                .handles
                .with(|table| claim.commit_with_storage(table)))
        });
        let (handles, retired_transfer_storage) = match result {
            Ok(handles) => handles,
            Err(error) => {
                return Err(HandleTransferCommitFailure {
                    error,
                    transfer: self,
                });
            }
        };
        drop(retired_transfer_storage);
        drop(self.entry_charge.take());
        self.release_scratch();
        let storage_charge = match self.handle_charge.take() {
            Some(charge) => charge,
            None => process_invariant_violation(),
        };
        self.process.reclaim_handle_pages();
        Ok(InTransitCapabilities::new(handles, storage_charge))
    }

    fn commit_pre_admitted(mut self) -> InTransitCapabilities {
        let (handles, retired_transfer_storage) = self.process.inner.state.with(|state| {
            for value in self.moved_values.iter().copied() {
                if !state.handle_accounting.is_live(value) {
                    process_invariant_violation();
                }
            }
            for value in self.moved_values.drain(..) {
                let (charge, retired_record) = state.handle_accounting.release(value);
                self.released_charges.push(charge);
                if let Some(record) = retired_record {
                    self.retired_records.push(record);
                }
            }
            let claim = match self.claim.take() {
                Some(claim) => claim,
                None => process_invariant_violation(),
            };
            self.process
                .inner
                .handles
                .with(|table| claim.commit_with_storage(table))
        });
        drop(retired_transfer_storage);
        drop(self.entry_charge.take());
        self.release_scratch();
        let storage_charge = match self.handle_charge.take() {
            Some(charge) => charge,
            None => process_invariant_violation(),
        };
        self.process.reclaim_handle_pages();
        InTransitCapabilities::new(handles, storage_charge)
    }
}

/// Fully prepared source and destination namespaces for one rendezvous.
///
/// Construction performs no table mutation beyond the two independently
/// reversible reservations. Commit locks both Processes by stable `ProcessId`,
/// validates admission, and then performs one allocation-free table and
/// accounting transaction. A recoverable admission failure retains this
/// complete owner for explicit rollback.
#[must_use = "commit or roll back the direct process handle transfer"]
pub(crate) struct PreparedDirectProcessHandleTransfer {
    source: Option<PreparedProcessHandleTransfer>,
    destination_process: Process,
    destination: Option<ProcessHandleBatchReservation>,
}

impl PreparedDirectProcessHandleTransfer {
    pub(crate) fn new(
        source: PreparedProcessHandleTransfer,
        destination_process: Process,
        destination: ProcessHandleBatchReservation,
    ) -> Self {
        if source.handle_count() != destination.values().len() {
            process_invariant_violation();
        }
        destination.require_owner(&destination_process);
        Self {
            source: Some(source),
            destination_process,
            destination: Some(destination),
        }
    }

    pub(crate) fn destination_values(&self) -> &[HandleValue] {
        match self.destination.as_ref() {
            Some(destination) => destination.values(),
            None => process_invariant_violation(),
        }
    }

    pub(crate) fn rollback(mut self) {
        let destination = match self.destination.take() {
            Some(destination) => destination,
            None => process_invariant_violation(),
        };
        self.destination_process.abort_handle_batch(destination);
        let source = match self.source.take() {
            Some(source) => source,
            None => process_invariant_violation(),
        };
        source.rollback();
    }

    /// Commits source removal, destination publication and quota transfer together.
    ///
    /// Process locks are ordered by ID, including the same-process case, and
    /// both lifecycles are checked before table mutation. Rejection returns the
    /// complete transaction for explicit rollback; retired storage is dropped
    /// only after the locks have been released.
    // The failure owns this complete linear transaction and is intentionally
    // large: allocating an error owner would make rollback fallible.
    #[allow(clippy::result_large_err)]
    pub(crate) fn commit(mut self) -> Result<(), DirectProcessHandleTransferCommitFailure> {
        let source_process = match self.source.as_ref() {
            Some(source) => source.process.clone(),
            None => process_invariant_violation(),
        };
        let destination_process = self.destination_process.clone();
        let source_id = source_process.id().get();
        let destination_id = destination_process.id().get();
        let result = if source_id == destination_id {
            source_process.inner.state.with(|state| {
                require_handle_phase(state.lifecycle.phase())?;
                let retired = source_process
                    .inner
                    .handles
                    .with(|table| commit_direct_same_process(&mut self, state, table));
                Ok(retired)
            })
        } else if source_id < destination_id {
            source_process.inner.state.with(|source_state| {
                destination_process.inner.state.with(|destination_state| {
                    require_handle_phase(source_state.lifecycle.phase())?;
                    require_handle_phase(destination_state.lifecycle.phase())?;
                    let retired = source_process.inner.handles.with(|source_table| {
                        destination_process.inner.handles.with(|destination_table| {
                            commit_direct_between_processes(
                                &mut self,
                                source_state,
                                source_table,
                                destination_state,
                                destination_table,
                            )
                        })
                    });
                    Ok(retired)
                })
            })
        } else {
            destination_process.inner.state.with(|destination_state| {
                source_process.inner.state.with(|source_state| {
                    require_handle_phase(source_state.lifecycle.phase())?;
                    require_handle_phase(destination_state.lifecycle.phase())?;
                    let retired = destination_process.inner.handles.with(|destination_table| {
                        source_process.inner.handles.with(|source_table| {
                            commit_direct_between_processes(
                                &mut self,
                                source_state,
                                source_table,
                                destination_state,
                                destination_table,
                            )
                        })
                    });
                    Ok(retired)
                })
            })
        };
        let retired = match result {
            Ok(retired) => retired,
            Err(error) => {
                return Err(DirectProcessHandleTransferCommitFailure {
                    error,
                    transfer: self,
                });
            }
        };
        drop(retired);
        finish_direct_process_transfer(&mut self);
        source_process.reclaim_handle_pages();
        destination_process.reclaim_handle_pages();
        Ok(())
    }
}

impl Drop for PreparedDirectProcessHandleTransfer {
    fn drop(&mut self) {
        if self.source.is_some() || self.destination.is_some() {
            process_invariant_violation();
        }
    }
}

#[must_use = "inspect the error and roll back the retained direct transfer"]
pub(crate) struct DirectProcessHandleTransferCommitFailure {
    #[expect(
        dead_code,
        reason = "Retains the commit failure cause alongside its rollback owner"
    )]
    pub(crate) error: ProcessError,
    pub(crate) transfer: PreparedDirectProcessHandleTransfer,
}

fn commit_direct_same_process(
    transfer: &mut PreparedDirectProcessHandleTransfer,
    state: &mut ProcessState,
    table: &mut HandleTable,
) -> RetiredDirectHandleTransfer {
    validate_direct_accounting(transfer, state);
    let direct = take_direct_table_transfer(transfer);
    let retired = direct.commit_within(table);
    commit_source_accounting(transfer, state);
    commit_destination_accounting(transfer, state);
    retired
}

fn commit_direct_between_processes(
    transfer: &mut PreparedDirectProcessHandleTransfer,
    source_state: &mut ProcessState,
    source_table: &mut HandleTable,
    destination_state: &mut ProcessState,
    destination_table: &mut HandleTable,
) -> RetiredDirectHandleTransfer {
    validate_direct_accounting(transfer, source_state);
    let direct = take_direct_table_transfer(transfer);
    let retired = direct.commit_between(source_table, destination_table);
    commit_source_accounting(transfer, source_state);
    commit_destination_accounting(transfer, destination_state);
    retired
}

fn validate_direct_accounting(
    transfer: &PreparedDirectProcessHandleTransfer,
    source_state: &ProcessState,
) {
    let source = match transfer.source.as_ref() {
        Some(source) => source,
        None => process_invariant_violation(),
    };
    if source.claim.is_none() {
        process_invariant_violation();
    }
    for value in source.moved_values.iter().copied() {
        if !source_state.handle_accounting.is_live(value) {
            process_invariant_violation();
        }
    }
    let destination = match transfer.destination.as_ref() {
        Some(destination) => destination,
        None => process_invariant_violation(),
    };
    let charges = match destination.handle_charges.as_ref() {
        Some(charges) => charges,
        None => process_invariant_violation(),
    };
    let record = match destination.record.as_ref() {
        Some(record) => record,
        None => process_invariant_violation(),
    };
    if charges.len() != destination.values().len()
        || record.state.with(|state| {
            state.entries.len() != charges.len()
                || state.entries.iter().any(|entry| entry.charge.is_some())
        })
    {
        process_invariant_violation();
    }
}

fn take_direct_table_transfer(
    transfer: &mut PreparedDirectProcessHandleTransfer,
) -> DirectHandleTransfer {
    let source_process = match transfer.source.as_ref() {
        Some(source) => source.process.id().get(),
        None => process_invariant_violation(),
    };
    let destination_process = transfer.destination_process.id().get();
    let source = match transfer.source.as_mut() {
        Some(source) => match source.claim.take() {
            Some(claim) => claim,
            None => process_invariant_violation(),
        },
        None => process_invariant_violation(),
    };
    let destination = match transfer.destination.as_mut() {
        Some(destination) => match destination.reservation.take() {
            Some(reservation) => reservation,
            None => process_invariant_violation(),
        },
        None => process_invariant_violation(),
    };
    DirectHandleTransfer::new(source_process, destination_process, source, destination)
}

fn commit_source_accounting(
    transfer: &mut PreparedDirectProcessHandleTransfer,
    source_state: &mut ProcessState,
) {
    let source = match transfer.source.as_mut() {
        Some(source) => source,
        None => process_invariant_violation(),
    };
    source.detach_source_charges(&mut source_state.handle_accounting);
}

fn commit_destination_accounting(
    transfer: &mut PreparedDirectProcessHandleTransfer,
    destination_state: &mut ProcessState,
) {
    let destination = match transfer.destination.as_mut() {
        Some(destination) => destination,
        None => process_invariant_violation(),
    };

    let mut charges = match destination.handle_charges.take() {
        Some(charges) => charges,
        None => process_invariant_violation(),
    };
    let record = match destination.record.take() {
        Some(record) => record,
        None => process_invariant_violation(),
    };
    destination_state
        .handle_accounting
        .install(record, &mut charges);
    drop(charges);
}

fn finish_direct_process_transfer(transfer: &mut PreparedDirectProcessHandleTransfer) {
    let mut source = match transfer.source.take() {
        Some(source) => source,
        None => process_invariant_violation(),
    };
    let mut destination = match transfer.destination.take() {
        Some(destination) => destination,
        None => process_invariant_violation(),
    };
    drop(source.entry_charge.take());
    drop(source.handle_charge.take());
    source.release_scratch();
    drop(destination.handle_charges.take());
    drop(destination.record.take());
    drop(destination.scratch_charge.take());
}

impl Drop for PreparedProcessHandleTransfer {
    fn drop(&mut self) {
        if self.claim.is_some()
            || self.entry_charge.is_some()
            || self.handle_charge.is_some()
            || self.scratch_charge.is_some()
            || !self.released_charges.is_empty()
            || !self.retired_records.is_empty()
            || !self.moved_values.is_empty()
        {
            process_invariant_violation();
        }
    }
}

/// Recoverable final-commit failure retaining the exact rollback owner.
#[must_use = "inspect the error and roll back the retained transfer"]
pub(crate) struct HandleTransferCommitFailure {
    pub(crate) error: ProcessError,
    pub(crate) transfer: PreparedProcessHandleTransfer,
}

#[must_use = "publish or abort the process handle reservation"]
pub(crate) struct ProcessHandleReservation<const N: usize> {
    pub(super) owner: ProcessId,
    pub(super) reservation: Option<HandleReservation<N>>,
    pub(super) handle_charges: Option<alloc::vec::Vec<ChargeReservation>>,
    pub(super) record: Option<FallibleArc<HandleChargeRecord>>,
}

#[must_use = "publish or abort the process handle batch reservation"]
pub(crate) struct ProcessHandleBatchReservation {
    pub(super) owner: ProcessId,
    pub(super) reservation: Option<HandleBatchReservation>,
    pub(super) handle_charges: Option<alloc::vec::Vec<ChargeReservation>>,
    pub(super) record: Option<FallibleArc<HandleChargeRecord>>,
    pub(super) scratch_charge: Option<CommittedCharge>,
}

impl<const N: usize> ProcessHandleReservation<N> {
    pub(super) fn require_owner(&self, process: &Process) {
        if self.owner != process.id() {
            process_invariant_violation();
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(
            dead_code,
            reason = "Used by the AArch64 Native self-tests; other HAL self-tests exercise different entry paths"
        )
    )]
    pub(crate) fn belongs_to(&self, process: &Process) -> bool {
        self.owner == process.id()
    }

    /// Future generation-tagged values which resolve only after publication.
    #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
    pub(crate) fn values(&self) -> [HandleValue; N] {
        match self.reservation.as_ref() {
            Some(reservation) => reservation.values(),
            None => process_invariant_violation(),
        }
    }
}

impl ProcessHandleBatchReservation {
    pub(super) fn require_owner(&self, process: &Process) {
        if self.owner != process.id() {
            process_invariant_violation();
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(
            dead_code,
            reason = "Used by the AArch64 Native self-tests; other HAL self-tests exercise different entry paths"
        )
    )]
    pub(crate) fn belongs_to(&self, process: &Process) -> bool {
        self.owner == process.id()
    }

    /// Future numeric values which remain unresolved until batch publication.
    pub(crate) fn values(&self) -> &[HandleValue] {
        match self.reservation.as_ref() {
            Some(reservation) => reservation.values(),
            None => process_invariant_violation(),
        }
    }
}

impl Drop for ProcessHandleBatchReservation {
    fn drop(&mut self) {
        if self.reservation.is_some()
            || self.handle_charges.is_some()
            || self.record.is_some()
            || self.scratch_charge.is_some()
        {
            process_invariant_violation();
        }
    }
}

impl<const N: usize> Drop for ProcessHandleReservation<N> {
    fn drop(&mut self) {
        if self.reservation.is_some() || self.handle_charges.is_some() || self.record.is_some() {
            process_invariant_violation();
        }
    }
}
