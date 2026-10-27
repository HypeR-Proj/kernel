// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process handle admission, quota-backed publication, and namespace operations.
//!
//! Each transaction keeps the existing Process-state -> handle-table lock order.
//! Prepared storage and detached owners are released only after both locks exit.

use super::super::lifecycle::{LifecycleError, ProcessPhase};
use super::super::user_thread::UserThread;
use super::handle_transactions::{
    HandlePublishFailure, PreparedHandleConsumption, PreparedProcessHandleTransfer,
    ProcessHandleBatchReservation, ProcessHandleReservation,
};
use super::{
    HandleChargeEntry, HandleChargeLocation, HandleChargeRecord, HandleChargeState, Process,
    ProcessError, ProcessLock, ProcessState, metadata_amount, process_invariant_violation,
};
use hyper::mm::FallibleArc;
use {
    crate::accounting::ChargeReservation, crate::accounting::CommittedCharge,
    crate::accounting::ResourceAmount, crate::accounting::ResourceKind,
};
use {
    crate::capability::ClosedHandle, crate::capability::HandleBatchReservation,
    crate::capability::HandleBatchReservationStorage, crate::capability::HandleError,
    crate::capability::HandleFlags, crate::capability::HandleInfo,
    crate::capability::HandleReservation, crate::capability::HandleScanCursor,
    crate::capability::HandleSidecar, crate::capability::HandleSidecarPlan,
    crate::capability::HandleSnapshotPage, crate::capability::HandleTable,
    crate::capability::HandleTableStoragePlan, crate::capability::HandleTableStorageSnapshot,
    crate::capability::HandleTransferClaim, crate::capability::HandleTransferRequest,
    crate::capability::HandleTransferRoute, crate::capability::HandleTransferStorage,
    crate::capability::HandleValue, crate::capability::InvocationReceiver,
    crate::capability::PreparedHandle, crate::capability::ResolvedObject,
    crate::capability::ResolvedWaitable, crate::capability::Rights,
};
use {
    crate::object::KernelObject, crate::object::Koid, crate::object::ObjectPublication,
    crate::object::UserExportableObject,
};

// Fields drop in declaration order on every retry and early return. Both
// storage owners must release their backing before the quota owner is dropped.
struct PreparedTableStorage {
    slots: Option<HandleTableStoragePlan>,
    index: HandleSidecarPlan<HandleChargeLocation>,
    charge: Option<CommittedCharge>,
}

#[derive(Clone, Copy)]
pub(super) enum HandleAdmission {
    Published,
    PreparedChild,
}

impl Process {
    pub(crate) fn reserve_handles<const N: usize>(
        &self,
    ) -> Result<ProcessHandleReservation<N>, ProcessError> {
        let reservation = loop {
            let snapshot = self.inner.state.with(|state| {
                require_handle_phase(state.lifecycle.phase())?;
                Ok::<_, ProcessError>(
                    self.inner
                        .handles
                        .with(|table| table.reservation_storage_snapshot_for(N))?,
                )
            })?;
            let mut storage = self.prepare_table_storage_plan(snapshot)?;
            let attempt = self.inner.state.with(|state| {
                require_handle_phase(state.lifecycle.phase())?;
                let current = self
                    .inner
                    .handles
                    .with(|table| table.reservation_storage_snapshot_for(N))?;
                if current != snapshot {
                    return Ok::<_, ProcessError>(None);
                }
                let reservation = self
                    .inner
                    .handles
                    .with(|table| table.reserve_with_plan(&mut storage.slots))?;
                install_table_storage_charge(state, snapshot, &mut storage.charge);
                state.handle_accounting.install_storage(&mut storage.index);
                Ok(Some(reservation))
            });
            match attempt {
                Ok(Some(reservation)) => break reservation,
                Ok(None) => drop(storage),
                Err(error) => return Err(error),
            }
        };
        let values = reservation.values();
        let entries_bytes = N
            .checked_mul(core::mem::size_of::<HandleChargeEntry>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let metadata_base = metadata_amount::<HandleChargeRecord>()?;
        let metadata_request = metadata_base.with(
            ResourceKind::KernelMemoryBytes,
            metadata_base
                .get(ResourceKind::KernelMemoryBytes)
                .checked_add(entries_bytes)
                .ok_or(ProcessError::Allocation)?,
        );
        let metadata_charge = match self.inner.domain.reserve(metadata_request) {
            Ok(charge) => charge.commit(),
            Err(error) => {
                self.abort_raw_handle_reservation(reservation);
                return Err(error.into());
            }
        };
        let mut handle_charges = alloc::vec::Vec::new();
        if handle_charges.try_reserve_exact(N).is_err() {
            self.abort_raw_handle_reservation(reservation);
            return Err(ProcessError::Allocation);
        }
        for _ in 0..N {
            let charge = match self
                .inner
                .domain
                .reserve(ResourceAmount::ZERO.with(ResourceKind::Handles, 1))
            {
                Ok(charge) => charge,
                Err(error) => {
                    self.abort_raw_handle_reservation(reservation);
                    return Err(error.into());
                }
            };
            handle_charges.push(charge);
        }
        let mut entries = alloc::vec::Vec::new();
        if entries.try_reserve_exact(N).is_err() {
            self.abort_raw_handle_reservation(reservation);
            return Err(ProcessError::Allocation);
        }
        for value in values {
            entries.push(HandleChargeEntry {
                value,
                charge: None,
            });
        }
        let record = match FallibleArc::try_new(HandleChargeRecord {
            previous: ProcessLock::new(None),
            state: ProcessLock::new(HandleChargeState { entries }),
            next: ProcessLock::new(None),
            _metadata_charge: metadata_charge,
        }) {
            Ok(record) => record,
            Err(_) => {
                self.abort_raw_handle_reservation(reservation);
                return Err(ProcessError::Allocation);
            }
        };
        Ok(ProcessHandleReservation {
            owner: self.id(),
            reservation: Some(reservation),
            handle_charges: Some(handle_charges),
            record: Some(record),
        })
    }

    pub(crate) fn reserve_handle_batch(
        &self,
        count: usize,
    ) -> Result<ProcessHandleBatchReservation, ProcessError> {
        self.reserve_handle_batch_for(count, HandleAdmission::Published)
    }

    pub(super) fn reserve_handle_batch_for(
        &self,
        count: usize,
        admission: HandleAdmission,
    ) -> Result<ProcessHandleBatchReservation, ProcessError> {
        HandleBatchReservationStorage::validate_count(count)?;
        let entries_bytes = count
            .checked_mul(core::mem::size_of::<HandleChargeEntry>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let metadata_base = metadata_amount::<HandleChargeRecord>()?;
        let metadata_request = metadata_base.with(
            ResourceKind::KernelMemoryBytes,
            metadata_base
                .get(ResourceKind::KernelMemoryBytes)
                .checked_add(entries_bytes)
                .ok_or(ProcessError::Allocation)?,
        );
        let charge_scratch_bytes = count
            .checked_mul(core::mem::size_of::<ChargeReservation>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let reservation_scratch_bytes = HandleBatchReservationStorage::allocation_size(count)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let scratch_bytes = charge_scratch_bytes
            .checked_add(reservation_scratch_bytes)
            .ok_or(ProcessError::Allocation)?;
        let scratch_request =
            ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, scratch_bytes);
        let scratch_charge = self.inner.domain.reserve(scratch_request)?.commit();
        let mut reservation_storage = Some(HandleBatchReservationStorage::try_new(count)?);
        let reservation = loop {
            let snapshot = self.inner.state.with(|state| {
                require_handle_admission(state.lifecycle.phase(), admission)?;
                Ok::<_, ProcessError>(
                    self.inner
                        .handles
                        .with(|table| table.reservation_storage_snapshot_for(count))?,
                )
            })?;
            let mut storage = self.prepare_table_storage_plan(snapshot)?;
            let attempt = self.inner.state.with(|state| {
                require_handle_admission(state.lifecycle.phase(), admission)?;
                let current = self
                    .inner
                    .handles
                    .with(|table| table.reservation_storage_snapshot_for(count))?;
                if current != snapshot {
                    return Ok::<_, ProcessError>(None);
                }
                let reservation = self.inner.handles.with(|table| {
                    table.reserve_batch_with_plan(
                        count,
                        &mut reservation_storage,
                        &mut storage.slots,
                    )
                })?;
                install_table_storage_charge(state, snapshot, &mut storage.charge);
                state.handle_accounting.install_storage(&mut storage.index);
                Ok(Some(reservation))
            });
            match attempt {
                Ok(Some(reservation)) => break reservation,
                Ok(None) => drop(storage),
                Err(error) => return Err(error),
            }
        };
        let metadata = self.inner.domain.reserve(metadata_request);
        let metadata = match metadata {
            Ok(charge) => charge.commit(),
            Err(error) => {
                self.abort_raw_handle_batch_reservation(reservation);
                return Err(error.into());
            }
        };
        let mut charges = alloc::vec::Vec::new();
        let mut entries = alloc::vec::Vec::new();
        if charges.try_reserve_exact(count).is_err() || entries.try_reserve_exact(count).is_err() {
            self.abort_raw_handle_batch_reservation(reservation);
            return Err(ProcessError::Allocation);
        }
        let mut charge_error = None;
        for value in reservation.values() {
            let charge = match self
                .inner
                .domain
                .reserve(ResourceAmount::ZERO.with(ResourceKind::Handles, 1))
            {
                Ok(charge) => charge,
                Err(error) => {
                    charge_error = Some(error);
                    break;
                }
            };
            charges.push(charge);
            entries.push(HandleChargeEntry {
                value: *value,
                charge: None,
            });
        }
        if let Some(error) = charge_error {
            self.abort_raw_handle_batch_reservation(reservation);
            return Err(error.into());
        }
        let record = match FallibleArc::try_new(HandleChargeRecord {
            previous: ProcessLock::new(None),
            state: ProcessLock::new(HandleChargeState { entries }),
            next: ProcessLock::new(None),
            _metadata_charge: metadata,
        }) {
            Ok(record) => record,
            Err(_) => {
                self.abort_raw_handle_batch_reservation(reservation);
                return Err(ProcessError::Allocation);
            }
        };
        Ok(ProcessHandleBatchReservation {
            owner: self.id(),
            reservation: Some(reservation),
            handle_charges: Some(charges),
            record: Some(record),
            scratch_charge: Some(scratch_charge),
        })
    }

    fn prepare_table_storage_plan(
        &self,
        snapshot: HandleTableStorageSnapshot,
    ) -> Result<PreparedTableStorage, ProcessError> {
        let storage_bytes = snapshot
            .growth_bytes()
            .and_then(|bytes| {
                bytes.checked_add(HandleSidecar::<HandleChargeLocation>::growth_bytes(
                    snapshot,
                )?)
            })
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let charge = if storage_bytes == 0 {
            None
        } else {
            Some(
                self.inner
                    .domain
                    .reserve(
                        ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, storage_bytes),
                    )?
                    .commit(),
            )
        };
        let plan = HandleTableStoragePlan::try_new(snapshot)?;
        let sidecar = HandleSidecar::prepare(snapshot)?;
        Ok(PreparedTableStorage {
            slots: Some(plan),
            index: sidecar,
            charge,
        })
    }

    pub(crate) fn publish_handles<const N: usize>(
        &self,
        mut reservation: ProcessHandleReservation<N>,
        handles: [PreparedHandle; N],
    ) -> Result<[HandleValue; N], HandlePublishFailure<N>> {
        reservation.require_owner(self);
        let mut handles = Some(handles);
        let mut retired_charge_storage = None;
        let result = self.inner.state.with(|state| {
            if require_handle_phase(state.lifecycle.phase()).is_err() {
                let token = match reservation.reservation.take() {
                    Some(token) => token,
                    None => process_invariant_violation(),
                };
                self.inner.handles.with(|table| token.abort(table));
                return Err(ProcessError::Lifecycle(LifecycleError::AdmissionClosed));
            }
            let token = match reservation.reservation.take() {
                Some(token) => token,
                None => process_invariant_violation(),
            };
            let prepared = match handles.take() {
                Some(handles) => handles,
                None => process_invariant_violation(),
            };
            let values = self
                .inner
                .handles
                .with(|table| token.publish(table, prepared));
            let mut charges = match reservation.handle_charges.take() {
                Some(charges) => charges,
                None => process_invariant_violation(),
            };
            let record = match reservation.record.take() {
                Some(record) => record,
                None => process_invariant_violation(),
            };
            state.handle_accounting.install(record, &mut charges);
            retired_charge_storage = Some(charges);
            Ok(values)
        });
        drop(retired_charge_storage.take());
        self.reclaim_handle_pages();
        match result {
            Ok(values) => Ok(values),
            Err(error) => {
                drop(reservation.handle_charges.take());
                drop(reservation.record.take());
                Err(HandlePublishFailure {
                    error,
                    handles: match handles.take() {
                        Some(handles) => handles,
                        None => process_invariant_violation(),
                    },
                })
            }
        }
    }

    pub(crate) fn abort_handle_batch(&self, mut reservation: ProcessHandleBatchReservation) {
        reservation.require_owner(self);
        let token = match reservation.reservation.take() {
            Some(token) => token,
            None => process_invariant_violation(),
        };
        let retired = self
            .inner
            .state
            .with(|_| self.inner.handles.with(|table| token.abort(table)));
        drop(retired);
        drop(reservation.handle_charges.take());
        drop(reservation.record.take());
        drop(reservation.scratch_charge.take());
        self.reclaim_handle_pages();
    }

    /// Narrows a prevalidated maximum receive reservation to the matched
    /// capability count without allocating or publishing a handle.
    ///
    /// A zero-sized match releases the complete reservation. Nonzero prefixes
    /// retain their original future values; unused tail values are generation
    /// advanced before becoming available to another syscall.
    pub(crate) fn trim_handle_batch(
        &self,
        mut reservation: ProcessHandleBatchReservation,
        count: usize,
    ) -> Option<ProcessHandleBatchReservation> {
        reservation.require_owner(self);
        if count == 0 {
            self.abort_handle_batch(reservation);
            return None;
        }
        if count > reservation.values().len() {
            process_invariant_violation();
        }
        if count == reservation.values().len() {
            return Some(reservation);
        }

        self.inner.state.with(|_| {
            let token = match reservation.reservation.as_mut() {
                Some(token) => token,
                None => process_invariant_violation(),
            };
            self.inner.handles.with(|table| token.trim_to(table, count));
            let record = match reservation.record.as_ref() {
                Some(record) => record,
                None => process_invariant_violation(),
            };
            record.state.with(|state| state.entries.truncate(count));
        });
        match reservation.handle_charges.as_mut() {
            Some(charges) => charges.truncate(count),
            None => process_invariant_violation(),
        }
        self.reclaim_handle_pages();
        Some(reservation)
    }

    pub(crate) fn abort_handles<const N: usize>(
        &self,
        mut reservation: ProcessHandleReservation<N>,
    ) {
        reservation.require_owner(self);
        let token = match reservation.reservation.take() {
            Some(token) => token,
            None => process_invariant_violation(),
        };
        self.abort_raw_handle_reservation(token);
        drop(reservation.handle_charges.take());
        drop(reservation.record.take());
    }

    fn abort_raw_handle_reservation<const N: usize>(&self, reservation: HandleReservation<N>) {
        self.inner.state.with(|_| {
            self.inner.handles.with(|table| reservation.abort(table));
        });
        self.reclaim_handle_pages();
    }

    fn abort_raw_handle_batch_reservation(&self, reservation: HandleBatchReservation) {
        let retired = self
            .inner
            .state
            .with(|_| self.inner.handles.with(|table| reservation.abort(table)));
        drop(retired);
        self.reclaim_handle_pages();
    }

    pub(crate) fn resolve_invocation(
        &self,
        value: HandleValue,
    ) -> Result<InvocationReceiver, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok(self
                .inner
                .handles
                .with(|table| table.resolve_invocation(value))?)
        })
    }

    pub(crate) fn resolve_handle<T: KernelObject>(
        &self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<ResolvedObject<T>, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok(self
                .inner
                .handles
                .with(|table| table.resolve(value, rights))?)
        })
    }

    pub(crate) fn resolve_waitable(
        &self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<ResolvedWaitable, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok(self
                .inner
                .handles
                .with(|table| table.resolve_waitable(value, rights))?)
        })
    }

    /// Claims source handles without changing their process-local values.
    ///
    /// The returned transaction owns every active capability while exact
    /// lookups report `Busy`. It can either restore the same values or perform
    /// one final generation-advancing move into an in-transit batch.
    pub(crate) fn prepare_handle_transfer(
        &self,
        requests: &[HandleTransferRequest],
        forbidden_object: Option<Koid>,
        forbidden_kind: Option<crate::object::ObjectKind>,
        route: HandleTransferRoute,
    ) -> Result<PreparedProcessHandleTransfer, ProcessError> {
        HandleTransferStorage::validate_count(requests.len())?;
        let entry_bytes = HandleTransferClaim::entry_allocation_size(requests.len())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let handle_bytes = HandleTransferClaim::handle_allocation_size(requests.len())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let scratch_bytes = requests
            .len()
            .checked_mul(
                core::mem::size_of::<CommittedCharge>()
                    .saturating_add(core::mem::size_of::<FallibleArc<HandleChargeRecord>>())
                    .saturating_add(core::mem::size_of::<HandleValue>()),
            )
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let entry_charge = self
            .inner
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, entry_bytes))?
            .commit();
        let handle_charge = self
            .inner
            .domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, handle_bytes)
                    .with(
                        ResourceKind::IpcHandles,
                        u64::try_from(requests.len()).map_err(|_| ProcessError::Allocation)?,
                    ),
            )?
            .commit();
        let scratch_charge = self
            .inner
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, scratch_bytes))?
            .commit();
        let mut storage = Some(HandleTransferStorage::try_new(requests.len())?);
        let mut released_charges = alloc::vec::Vec::new();
        released_charges
            .try_reserve_exact(requests.len())
            .map_err(|_| ProcessError::Allocation)?;
        let mut retired_records = alloc::vec::Vec::new();
        retired_records
            .try_reserve_exact(requests.len())
            .map_err(|_| ProcessError::Allocation)?;
        let mut moved_values = alloc::vec::Vec::new();
        moved_values
            .try_reserve_exact(requests.len())
            .map_err(|_| ProcessError::Allocation)?;
        let claim = self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok::<_, ProcessError>(self.inner.handles.with(|table| {
                table.prepare_transfer_with_storage(
                    requests,
                    forbidden_object,
                    forbidden_kind,
                    route,
                    &mut storage,
                )
            })?)
        })?;
        moved_values.extend(claim.values());
        Ok(PreparedProcessHandleTransfer {
            process: self.clone(),
            claim: Some(claim),
            entry_charge: Some(entry_charge),
            handle_charge: Some(handle_charge),
            scratch_charge: Some(scratch_charge),
            released_charges,
            retired_records,
            moved_values,
        })
    }

    /// Reversibly claims one typed handle for consume-on-success lifecycle use.
    ///
    /// Unlike capability transfer, this path requires an object operation
    /// right and no propagation right. `expected_koid` binds the claim to the
    /// object resolved by the syscall adapter before entering the coordinator.
    /// The caller retains that resolved object until this claim is established;
    /// the KOID comparison never recovers authority from a historical ID.
    pub(crate) fn prepare_handle_consumption(
        &self,
        value: HandleValue,
        required: Rights,
        expected_kind: crate::object::ObjectKind,
        expected_koid: Koid,
    ) -> Result<PreparedHandleConsumption, ProcessError> {
        let entry_bytes = HandleTransferClaim::entry_allocation_size(1)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let handle_bytes = HandleTransferClaim::handle_allocation_size(1)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProcessError::Allocation)?;
        let scratch_bytes = core::mem::size_of::<CommittedCharge>()
            .saturating_add(core::mem::size_of::<FallibleArc<HandleChargeRecord>>())
            .saturating_add(core::mem::size_of::<HandleValue>());
        let scratch_bytes = u64::try_from(scratch_bytes).map_err(|_| ProcessError::Allocation)?;
        let entry_charge = self
            .inner
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, entry_bytes))?
            .commit();
        let handle_charge = self
            .inner
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, handle_bytes))?
            .commit();
        let scratch_charge = self
            .inner
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, scratch_bytes))?
            .commit();
        let mut storage = Some(HandleTransferStorage::try_new(1)?);
        let mut released_charges = alloc::vec::Vec::new();
        released_charges
            .try_reserve_exact(1)
            .map_err(|_| ProcessError::Allocation)?;
        let mut retired_records = alloc::vec::Vec::new();
        retired_records
            .try_reserve_exact(1)
            .map_err(|_| ProcessError::Allocation)?;
        let mut moved_values = alloc::vec::Vec::new();
        moved_values
            .try_reserve_exact(1)
            .map_err(|_| ProcessError::Allocation)?;
        let claim = self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok::<_, ProcessError>(self.inner.handles.with(|table| {
                table.prepare_consumption_with_storage(
                    value,
                    required,
                    expected_kind,
                    expected_koid,
                    &mut storage,
                )
            })?)
        })?;
        moved_values.push(value);
        Ok(PreparedHandleConsumption {
            transfer: Some(PreparedProcessHandleTransfer {
                process: self.clone(),
                claim: Some(claim),
                entry_charge: Some(entry_charge),
                handle_charge: Some(handle_charge),
                scratch_charge: Some(scratch_charge),
                released_charges,
                retired_records,
                moved_values,
            }),
        })
    }

    /// Publishes the first process-local handle for a new kernel object.
    ///
    /// Slot, quota, object identity, and active-handle state are prepared
    /// before the Process lock commits publication. Every failure before that
    /// point rolls back both the slot reservation and unpublished authority.
    pub(crate) fn create_object<T: UserExportableObject>(
        &self,
        payload: T,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        let reservation = self.reserve_handles::<1>()?;
        self.publish_reserved_object(reservation, payload, rights)
    }

    /// Publishes a new object into a slot reserved before fallible payload
    /// construction began.
    ///
    /// Stateful payload factories use this ordering when dropping an
    /// unpublished payload cannot itself undo an external logical mutation.
    pub(crate) fn publish_reserved_object<T: UserExportableObject>(
        &self,
        reservation: ProcessHandleReservation<1>,
        payload: T,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        let object = match ObjectPublication::try_new(payload) {
            Ok(object) => object,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error.into());
            }
        };
        let prepared = match PreparedHandle::try_from_new_object(object, rights, HandleFlags::NONE)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error.into());
            }
        };
        match self.publish_handles(reservation, [prepared]) {
            Ok(values) => Ok(values[0]),
            Err(failure) => Err(failure.error),
        }
    }

    /// Publishes this Process's first userspace authority to an existing thread.
    ///
    /// The thread already has a canonical object identity. This transaction
    /// mints its one initial handle without constructing a second wrapper or
    /// allowing authority resurrection after the final handle closes.
    pub(crate) fn publish_thread_handle(
        &self,
        thread: &UserThread,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        if thread.process_id() != self.id() {
            return Err(ProcessError::Handle(HandleError::AccessDenied));
        }
        let reservation = self.reserve_handles::<1>()?;
        let prepared = match PreparedHandle::try_from_new_object(
            thread.publication(),
            rights,
            HandleFlags::NONE,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error.into());
            }
        };
        match self.publish_handles(reservation, [prepared]) {
            Ok(values) => Ok(values[0]),
            Err(failure) => Err(failure.error),
        }
    }

    /// Publishes a same-kind object pair in one handle-table transaction.
    ///
    /// Both erased objects and both active owners exist before publication, so
    /// userspace can never observe only one endpoint of a newly created pair.
    pub(crate) fn create_object_pair<T: crate::object::diagnostics::PairedObject>(
        &self,
        first: T,
        second: T,
        rights: Rights,
    ) -> Result<[HandleValue; 2], ProcessError> {
        let reservation = self.reserve_handles::<2>()?;
        let first = match ObjectPublication::try_new(first) {
            Ok(object) => object,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error.into());
            }
        };
        let second = match ObjectPublication::try_new(second) {
            Ok(object) => object,
            Err(error) => {
                self.abort_handles(reservation);
                drop(first);
                return Err(error.into());
            }
        };
        first.object().bind_peer_identity(second.koid());
        second.object().bind_peer_identity(first.koid());
        let first = match PreparedHandle::try_from_new_object(first, rights, HandleFlags::NONE) {
            Ok(handle) => handle,
            Err(error) => {
                self.abort_handles(reservation);
                drop(second);
                return Err(error.into());
            }
        };
        let second = match PreparedHandle::try_from_new_object(second, rights, HandleFlags::NONE) {
            Ok(handle) => handle,
            Err(error) => {
                self.abort_handles(reservation);
                drop(first);
                return Err(error.into());
            }
        };
        match self.publish_handles(reservation, [first, second]) {
            Ok(values) => Ok(values),
            Err(failure) => Err(failure.error),
        }
    }

    pub(crate) fn handle_info(
        &self,
        value: HandleValue,
        required_rights: Rights,
    ) -> Result<HandleInfo, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            let info = self.inner.handles.with(|table| table.get_info(value))?;
            if !info.rights.contains(required_rights) {
                return Err(ProcessError::Handle(HandleError::AccessDenied));
            }
            Ok(info)
        })
    }

    /// Returns one bounded, authority-free page of this Process handle graph.
    pub(crate) fn scan_handles(
        &self,
        cursor: HandleScanCursor,
    ) -> Result<HandleSnapshotPage, ProcessError> {
        self.inner
            .handles
            .with(|table| table.scan_handles(cursor))
            .map_err(Into::into)
    }

    pub(crate) fn inspect_handle_object(
        &self,
        value: HandleValue,
    ) -> Result<crate::object::ErasedKernelRef<crate::object::Diagnostic>, ProcessError> {
        self.inner
            .handles
            .with(|table| table.inspect_object(value))
            .map_err(Into::into)
    }

    pub(crate) fn derive_process_observer(
        &self,
        value: HandleValue,
    ) -> Result<HandleValue, ProcessError> {
        let reservation = self.reserve_handles::<1>()?;
        let prepared = self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok::<_, ProcessError>(
                self.inner
                    .handles
                    .with(|table| table.derive_process_observer(value))?,
            )
        });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error);
            }
        };
        match self.publish_handles(reservation, [prepared]) {
            Ok(values) => Ok(values[0]),
            Err(failure) => Err(failure.error),
        }
    }

    pub(crate) fn duplicate_handle(
        &self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        let reservation = self.reserve_handles::<1>()?;
        let prepared = self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            Ok::<_, ProcessError>(
                self.inner
                    .handles
                    .with(|table| table.duplicate(value, rights))?,
            )
        });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.abort_handles(reservation);
                return Err(error);
            }
        };
        match self.publish_handles(reservation, [prepared]) {
            Ok(values) => Ok(values[0]),
            Err(failure) => Err(failure.error),
        }
    }

    pub(crate) fn replace_handle(
        &self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<HandleValue, ProcessError> {
        self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            let replacement = self
                .inner
                .handles
                .with(|table| table.replace(value, rights))?;
            state.handle_accounting.replace(value, replacement);
            Ok(replacement)
        })
    }

    pub(crate) fn close_handle(&self, value: HandleValue) -> Result<(), ProcessError> {
        let (closed, charge, retired_record): (
            ClosedHandle,
            CommittedCharge,
            Option<FallibleArc<HandleChargeRecord>>,
        ) = self.inner.state.with(|state| {
            require_handle_phase(state.lifecycle.phase())?;
            let closed = self.inner.handles.with(|table| table.remove(value))?;
            let (charge, retired_record) = state.handle_accounting.release(value);
            Ok::<_, ProcessError>((closed, charge, retired_record))
        })?;
        closed.complete();
        drop(charge);
        drop(retired_record);
        self.reclaim_handle_pages();
        Ok(())
    }

    /// Releases complete empty page pairs after namespace transactions leave
    /// their locks. Directory/generation metadata remains charged. A bounded
    /// batch avoids chasing concurrent producers indefinitely; each producer
    /// also drains the pages made empty by its own bounded operation.
    #[inline(never)]
    pub(super) fn reclaim_handle_pages(&self) {
        for _ in 0..64 {
            let detached = self.inner.state.with(|state| {
                let page = self.inner.handles.with(HandleTable::take_empty_page)?;
                let sidecar = state.handle_accounting.detach_empty_page(page.index());
                let amount = ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, page.backing_bytes() as u64);
                let charge = match state.handle_table_charge.as_mut() {
                    Some(total) => total.split_off(amount),
                    None => process_invariant_violation(),
                };
                Some((page, sidecar, charge))
            });
            let Some((page, sidecar, charge)) = detached else {
                break;
            };
            drop(page);
            drop(sidecar);
            // Keep quota conservative until both physical pages are returned.
            drop(charge);
        }
    }
}

fn install_table_storage_charge(
    state: &mut ProcessState,
    snapshot: HandleTableStorageSnapshot,
    prepared: &mut Option<CommittedCharge>,
) {
    let bytes = match snapshot.growth_bytes() {
        Some(bytes) => bytes,
        None => process_invariant_violation(),
    };
    if bytes == 0 {
        if prepared.is_some() {
            process_invariant_violation();
        }
    } else {
        let charge = match prepared.take() {
            Some(charge) => charge,
            None => process_invariant_violation(),
        };
        // Every extension was admitted against this Process's domain before
        // storage publication. Coalescing transfers the existing charge; it
        // neither reserves quota again nor releases it. Empty-page reclamation
        // and final table retirement retain ownership until backing destruction.
        match state.handle_table_charge.as_mut() {
            Some(total) => total.absorb_pre_admitted(charge),
            None => state.handle_table_charge = Some(charge),
        }
    }
}

pub(super) fn require_handle_phase(phase: ProcessPhase) -> Result<(), ProcessError> {
    require_handle_admission(phase, HandleAdmission::Published)
}

pub(super) fn require_handle_admission(
    phase: ProcessPhase,
    admission: HandleAdmission,
) -> Result<(), ProcessError> {
    let admitted = match admission {
        HandleAdmission::Published => {
            matches!(phase, ProcessPhase::Created | ProcessPhase::Running)
        }
        HandleAdmission::PreparedChild => phase == ProcessPhase::Prepared,
    };
    if admitted {
        Ok(())
    } else {
        Err(ProcessError::Lifecycle(LifecycleError::AdmissionClosed))
    }
}
