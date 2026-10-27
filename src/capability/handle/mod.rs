// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Opaque process-local handles and unpublished slot reservations.

use alloc::vec::Vec;
use core::array;
use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};

use hyper::abi::native::{
    HYPER_NATIVE_HANDLE_PREFIX as HANDLE_PREFIX,
    HYPER_NATIVE_HANDLE_PREFIX_MASK as HANDLE_PREFIX_MASK,
};

use super::Rights;
use {crate::authority::HandleRights, crate::authority::PropagationRights};
use {
    crate::object::ActiveHandleError, crate::object::ActiveHandleOwner, crate::object::Diagnostic,
    crate::object::ErasedKernelRef, crate::object::KernelObject, crate::object::Koid,
    crate::object::ObjectKind, crate::object::ObjectPublication, crate::object::ObjectRetirement,
    crate::object::OperationPin, crate::object::SignalSource, crate::object::TransferClass,
    crate::object::UserExportableObject,
};

const SLOT_BITS: u32 = 24;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
const PAYLOAD_MASK: u64 = !HANDLE_PREFIX_MASK;
const GENERATION_LIMIT: u64 = PAYLOAD_MASK >> SLOT_BITS;
const MAX_SLOTS: usize = SLOT_MASK as usize;
const MAX_RESERVATION_SLOTS: usize = 64;
const DIAGNOSTIC_PAGE_CAPACITY: usize = 8;
const DIAGNOSTIC_SLOT_BUDGET: usize = 256;

/// Generations reduce accidental stale-value reuse; they are not eternal
/// identities. Closed values may repeat after wrap. Zero is never published.
const fn next_generation(generation: u64) -> u64 {
    if generation == GENERATION_LIMIT {
        1
    } else {
        generation + 1
    }
}

mod page;
mod storage;
pub(crate) use storage::{
    HandleSidecar, HandleSidecarPlan, HandleTableStoragePlan, HandleTableStorageSnapshot,
    RetiredHandleStorage,
};
use storage::{RetiredHandlePage, SlotStore};

static NEXT_RESERVATION_ID: AtomicU64 = AtomicU64::new(1);

fn validate_batch_count(count: usize) -> Result<(), HandleError> {
    if count == 0 {
        Err(HandleError::EmptyReservation)
    } else if count > MAX_RESERVATION_SLOTS {
        Err(HandleError::ReservationTooLarge)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ReservationId(NonZeroU64);

impl ReservationId {
    fn allocate() -> Result<Self, HandleError> {
        let mut current = NEXT_RESERVATION_ID.load(Ordering::Relaxed);
        loop {
            let value = NonZeroU64::new(current).ok_or(HandleError::ReservationIdExhausted)?;
            let next = current
                .checked_add(1)
                .ok_or(HandleError::ReservationIdExhausted)?;
            match NEXT_RESERVATION_ID.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(Self(value)),
                Err(observed) => current = observed,
            }
        }
    }
}

/// Nonzero opaque handle with the ABI's 0x0F prefix, interpreted by its owning Process.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct HandleValue(NonZeroU64);

/// Table-local encoding masks all 56 payload bits. The separate 0x0F prefix
/// guarantees nonzero values even when XOR clears the entire payload.
/// Slot/generation fields and the mask are private, not security secrets.
#[derive(Clone, Copy, Eq, PartialEq)]
struct HandleCodec {
    mask: u64,
}

impl HandleCodec {
    const fn new(seed: u64) -> Self {
        let mask = seed & PAYLOAD_MASK;
        Self {
            mask: if mask == 0 { 1 } else { mask },
        }
    }

    fn encode(self, slot: usize, generation: u64) -> HandleValue {
        if slot >= MAX_SLOTS || generation == 0 || generation > GENERATION_LIMIT {
            unreachable_handle_value();
        }
        let payload = (generation << SLOT_BITS) | (slot as u64 + 1);
        let raw = HANDLE_PREFIX | (payload ^ self.mask);
        match NonZeroU64::new(raw) {
            Some(value) => HandleValue(value),
            None => unreachable_handle_value(),
        }
    }

    fn decode(self, value: HandleValue) -> Result<(usize, u64), HandleError> {
        let raw = (value.0.get() & PAYLOAD_MASK) ^ self.mask;
        let slot = raw & SLOT_MASK;
        let generation = raw >> SLOT_BITS;
        if slot == 0 || generation == 0 {
            return Err(HandleError::InvalidHandle);
        }
        Ok((slot as usize - 1, generation))
    }

    /// For values already validated under the same exclusive table borrow.
    fn require_decoded(self, value: HandleValue) -> (usize, u64) {
        match self.decode(value) {
            Ok(decoded) => decoded,
            Err(_) => unreachable_handle_value(),
        }
    }
}

pub(crate) mod wire;

#[cfg(test)]
mod encoding_tests;

#[cold]
fn unreachable_handle_value() -> ! {
    super::invariant_violation()
}

/// Currently supported per-handle flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HandleFlags(u32);

impl HandleFlags {
    pub(crate) const NONE: Self = Self(0);

    pub(crate) const fn from_bits(bits: u32) -> Option<Self> {
        if bits == 0 { Some(Self(bits)) } else { None }
    }

    pub(crate) const fn bits(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleError {
    Allocation,
    InvalidHandle,
    Busy,
    WrongObjectType,
    AccessDenied,
    UnsupportedRights,
    UnsupportedFlags,
    UnsupportedTransfer,
    ObjectRetired,
    ObjectAlreadyActive,
    ActiveHandleLimit,
    ReservationIdExhausted,
    ReservationTooLarge,
    OutstandingReservation,
    TableFull,
    TableRetired,
    EmptyReservation,
}

/// Handle-local metadata returned without exposing the object payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleInfo {
    pub(crate) koid: Koid,
    pub(crate) kind: ObjectKind,
    pub(crate) rights: Rights,
    pub(crate) flags: HandleFlags,
}

/// Position in a process-local handle-table diagnostic scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleScanCursor {
    next_slot: usize,
}

impl HandleScanCursor {
    pub(crate) const fn from_token(token: usize) -> Self {
        Self { next_slot: token }
    }

    pub(crate) const fn token(self) -> usize {
        self.next_slot
    }
}

/// One handle-table edge from a process-local value to a kernel object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleSnapshot {
    pub(crate) value: HandleValue,
    pub(crate) info: HandleInfo,
}

/// Bounded diagnostic output from one locked handle-table observation.
pub(crate) struct HandleSnapshotPage {
    entries: [Option<HandleSnapshot>; DIAGNOSTIC_PAGE_CAPACITY],
    len: usize,
    next: Option<HandleScanCursor>,
}

impl HandleSnapshotPage {
    pub(crate) fn entries(&self) -> impl Iterator<Item = &HandleSnapshot> {
        self.entries[..self.len].iter().filter_map(Option::as_ref)
    }

    pub(crate) const fn next(&self) -> Option<HandleScanCursor> {
        self.next
    }
}

/// Ownership operation requested for one capability transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleTransferOperation {
    Move,
    Copy,
}

/// Storage contract of the transport receiving the capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleTransferRoute {
    /// The transport may retain capability owners after the sender returns.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "Retaining transports are rejected by production policy and exercised by host and kernel tests"
        )
    )]
    Buffered,
    /// The source and destination namespaces commit directly while paired.
    Rendezvous,
    /// Authority is staged in a bounded, explicitly type-audited startup
    /// container before publication into a child namespace.
    StagedStartup,
}

impl HandleTransferRoute {
    const fn permits(self, class: TransferClass) -> bool {
        match (self, class) {
            (_, TransferClass::Leaf)
            | (Self::Rendezvous | Self::StagedStartup, TransferClass::RendezvousOnly) => true,
            (_, TransferClass::Never) | (Self::Buffered, TransferClass::RendezvousOnly) => false,
        }
    }
}

/// One source handle and its attenuated rights in a transfer transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleTransferRequest {
    pub(crate) value: HandleValue,
    /// Maximum authority offered by the sender. `None` means the complete
    /// source-handle rights set, equivalent to an ABI `SAME_RIGHTS` request.
    pub(crate) offered_rights: Option<Rights>,
    /// Exact authority installed at the destination.
    pub(crate) rights: Rights,
    /// Optional sender-side assertion about the source object kind.
    pub(crate) offered_kind: Option<ObjectKind>,
    /// Exact receiver-side object-kind contract.
    pub(crate) expected_kind: Option<ObjectKind>,
    pub(crate) operation: HandleTransferOperation,
}

/// One active but not necessarily published process handle.
///
/// Prepared handles are active authority. This keeps duplicate rollback and
/// future in-transit capability ownership from producing a false zero-active
/// transition. Dropping an unpublished value rolls that authority back and may
/// run the zero-active callback; a potentially final owner must therefore be
/// dropped only after releasing Process and object locks.
pub(crate) struct PreparedHandle {
    object: Option<ActiveHandleOwner>,
    rights: HandleRights,
    flags: HandleFlags,
}

impl PreparedHandle {
    /// Mints the sole first handle for a newly constructed object.
    pub(crate) fn try_from_new_object<T: UserExportableObject>(
        publication: ObjectPublication<T>,
        rights: Rights,
        flags: HandleFlags,
    ) -> Result<Self, HandleError> {
        if !publication.supported_rights().contains(rights) {
            return Err(HandleError::UnsupportedRights);
        }
        if HandleFlags::from_bits(flags.bits()).is_none() {
            return Err(HandleError::UnsupportedFlags);
        }
        let object = publication.activate().map_err(|error| match error {
            ActiveHandleError::NotExportable => HandleError::UnsupportedRights,
            ActiveHandleError::Retired => HandleError::ObjectRetired,
            ActiveHandleError::AlreadyActive => HandleError::ObjectAlreadyActive,
            ActiveHandleError::CountExhausted => HandleError::ActiveHandleLimit,
        })?;
        Ok(Self {
            object: Some(object),
            rights: rights.decompose(),
            flags,
        })
    }

    pub(crate) fn try_duplicate(&self, rights: Rights) -> Result<Self, HandleError> {
        let rights = rights.decompose();
        if !self.rights.contains(rights) {
            return Err(HandleError::AccessDenied);
        }
        let object = self.object().try_duplicate().map_err(|error| match error {
            ActiveHandleError::NotExportable => super::invariant_violation(),
            ActiveHandleError::Retired => HandleError::ObjectRetired,
            ActiveHandleError::AlreadyActive => super::invariant_violation(),
            ActiveHandleError::CountExhausted => HandleError::ActiveHandleLimit,
        })?;
        Ok(Self {
            object: Some(object),
            rights,
            flags: self.flags,
        })
    }

    #[cfg(test)]
    pub(crate) fn duplicate_for_test(&self, rights: Rights) -> Result<Self, HandleError> {
        self.try_duplicate(rights)
    }

    fn object(&self) -> &ActiveHandleOwner {
        match self.object.as_ref() {
            Some(object) => object,
            None => super::invariant_violation(),
        }
    }

    fn release_into(&mut self, retirement: &mut ObjectRetirement) {
        let object = match self.object.take() {
            Some(object) => object,
            None => super::invariant_violation(),
        };
        object.release_into(retirement);
    }
}

impl Drop for PreparedHandle {
    fn drop(&mut self) {
        if self.object.is_none() {
            return;
        }
        let mut retirement = ObjectRetirement::new();
        self.release_into(&mut retirement);
        retirement.drain();
    }
}

enum Slot {
    Vacant {
        generation: u64,
        next_free: Option<usize>,
        previous_free: Option<usize>,
    },
    Reserved {
        generation: u64,
        reservation: ReservationId,
    },
    TransferReserved {
        generation: u64,
        transfer: ReservationId,
    },
    Occupied {
        generation: u64,
        handle: PreparedHandle,
    },
    Empty,
}

/// Unsynchronized table state owned and locked by one Process.
pub(crate) struct HandleTable {
    codec: HandleCodec,
    slots: SlotStore,
    free_head: Option<usize>,
    free_slots: usize,
    active_transfers: usize,
    lifecycle: TableLifecycle,
    next_teardown_generation: u64,
}

#[derive(Clone, Copy)]
enum TableLifecycle {
    Active,
    TearingDown { generation: u64 },
    Retired,
}

impl HandleTable {
    /// Fixes this namespace's encoding for its entire lifetime. Production
    /// callers supply one seed from the kernel generator at process creation.
    pub(crate) const fn new(seed: u64) -> Self {
        Self {
            codec: HandleCodec::new(seed),
            slots: SlotStore::new(),
            free_head: None,
            free_slots: 0,
            active_transfers: 0,
            lifecycle: TableLifecycle::Active,
            next_teardown_generation: 1,
        }
    }

    /// Creates an accounting index in this exact namespace without exporting
    /// its codec or slot numbering to the Process owner.
    pub(crate) fn new_sidecar<T>(&self) -> HandleSidecar<T> {
        HandleSidecar::new(self.codec)
    }

    /// Returns the persistent logical-slot growth required by one reservation.
    #[cfg(test)]
    pub(crate) fn reservation_growth<const N: usize>(&self) -> Result<usize, HandleError> {
        self.reservation_growth_for(N)
    }

    #[cfg(test)]
    pub(crate) fn reservation_growth_for(&self, count: usize) -> Result<usize, HandleError> {
        self.ensure_active()?;
        validate_batch_count(count)?;
        let additional = count.saturating_sub(self.free_slots);
        self.slots.snapshot(count, self.free_slots)?;
        Ok(additional)
    }

    pub(crate) fn reservation_storage_snapshot_for(
        &self,
        count: usize,
    ) -> Result<HandleTableStorageSnapshot, HandleError> {
        self.ensure_active()?;
        validate_batch_count(count)?;
        self.slots.snapshot(count, self.free_slots)
    }

    #[cfg(test)]
    pub(crate) fn reserve_batch(
        &mut self,
        count: usize,
    ) -> Result<HandleBatchReservation, HandleError> {
        let storage = HandleBatchReservationStorage::try_new(count)?;
        let snapshot = self.reservation_storage_snapshot_for(count)?;
        let plan = HandleTableStoragePlan::try_new(snapshot)?;
        let mut storage = Some(storage);
        let mut plan = Some(plan);
        self.reserve_batch_with_plan(count, &mut storage, &mut plan)
    }

    /// Reserves a runtime-sized batch from externally prepared storage.
    pub(crate) fn reserve_batch_with_plan(
        &mut self,
        count: usize,
        storage: &mut Option<HandleBatchReservationStorage>,
        plan: &mut Option<HandleTableStoragePlan>,
    ) -> Result<HandleBatchReservation, HandleError> {
        self.ensure_active()?;
        let prepared_storage = match storage.as_ref() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        if prepared_storage.count != count
            || !prepared_storage.slots.is_empty()
            || !prepared_storage.values.is_empty()
        {
            super::invariant_violation();
        }
        let prepared = match plan.as_ref() {
            Some(plan) => plan,
            None => super::invariant_violation(),
        };
        if self.reservation_storage_snapshot_for(count)? != prepared.snapshot() {
            super::invariant_violation();
        }
        let reservation = ReservationId::allocate()?;
        let plan = match plan.as_mut() {
            Some(plan) => plan,
            None => super::invariant_violation(),
        };
        self.install_storage_plan(plan);
        let mut storage = match storage.take() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        for _ in 0..count {
            let (index, generation) = self.reserve_free_slot(reservation);
            storage.slots.push(ReservedSlot { index, generation });
            storage.values.push(self.codec.encode(index, generation));
        }
        Ok(HandleBatchReservation {
            reservation,
            slots: storage.slots,
            values: storage.values,
            completed: false,
        })
    }

    /// Reserves `N` unresolvable slots for one final publication transaction.
    ///
    /// Existing vacant slots are removed from an intrusive free list, so the
    /// non-allocation path costs O(N) rather than O(total table slots).
    #[cfg(test)]
    pub(crate) fn reserve<const N: usize>(&mut self) -> Result<HandleReservation<N>, HandleError> {
        let snapshot = self.reservation_storage_snapshot_for(N)?;
        let plan = HandleTableStoragePlan::try_new(snapshot)?;
        let mut plan = Some(plan);
        self.reserve_with_plan(&mut plan)
    }

    /// Reserves a fixed-size batch from externally prepared table storage.
    pub(crate) fn reserve_with_plan<const N: usize>(
        &mut self,
        plan: &mut Option<HandleTableStoragePlan>,
    ) -> Result<HandleReservation<N>, HandleError> {
        self.ensure_active()?;
        let prepared = match plan.as_ref() {
            Some(plan) => plan,
            None => super::invariant_violation(),
        };
        if self.reservation_storage_snapshot_for(N)? != prepared.snapshot() {
            super::invariant_violation();
        }
        let reservation = ReservationId::allocate()?;
        let plan = match plan.as_mut() {
            Some(plan) => plan,
            None => super::invariant_violation(),
        };
        self.install_storage_plan(plan);

        let mut selected = [0; N];
        let mut generations = [0; N];
        for position in 0..N {
            let (index, generation) = self.reserve_free_slot(reservation);
            selected[position] = index;
            generations[position] = generation;
        }
        Ok(HandleReservation {
            codec: self.codec,
            reservation,
            slots: selected,
            generations,
            completed: false,
        })
    }

    /// Installs a preallocated replacement and grows only within proven capacity.
    fn install_storage_plan(&mut self, plan: &mut HandleTableStoragePlan) {
        let (pages, count) = self.slots.install(plan);
        for &page in &pages[..count] {
            let generation = self.slots.fresh_generation(page);
            for index in self.slots.page_range(page) {
                self.publish_vacant_slot(index, generation);
            }
        }
    }

    fn unlink_free_slot(&mut self, index: usize) -> u64 {
        let (generation, previous, next) = match self.slots.get(index) {
            Some(Slot::Vacant {
                generation,
                previous_free,
                next_free,
            }) => (*generation, *previous_free, *next_free),
            _ => super::invariant_violation(),
        };
        match previous {
            Some(previous) => match self.slots.get_mut(previous) {
                Some(Slot::Vacant { next_free, .. }) => *next_free = next,
                _ => super::invariant_violation(),
            },
            None => self.free_head = next,
        }
        if let Some(next) = next {
            match self.slots.get_mut(next) {
                Some(Slot::Vacant { previous_free, .. }) => *previous_free = previous,
                _ => super::invariant_violation(),
            }
        }
        if self.free_slots == 0 {
            super::invariant_violation();
        }
        self.free_slots -= 1;
        generation
    }

    fn reserve_free_slot(&mut self, reservation: ReservationId) -> (usize, u64) {
        let index = match self.free_head {
            Some(index) => index,
            None => super::invariant_violation(),
        };
        let generation = self.unlink_free_slot(index);
        self.slots.replace(
            index,
            Slot::Reserved {
                generation,
                reservation,
            },
        );
        (index, generation)
    }

    fn publish_vacant_slot(&mut self, index: usize, generation: u64) {
        if generation == 0 || generation > GENERATION_LIMIT {
            super::invariant_violation();
        }
        if let Some(head) = self.free_head {
            match self.slots.get_mut(head) {
                Some(Slot::Vacant { previous_free, .. }) => *previous_free = Some(index),
                _ => super::invariant_violation(),
            }
        }
        self.slots.replace(
            index,
            Slot::Vacant {
                generation,
                next_free: self.free_head,
                previous_free: None,
            },
        );
        self.free_head = Some(index);
        self.free_slots += 1;
    }

    /// Detaches at most one completely unoccupied page. Every free-list edge
    /// is removed in page-bounded work; no live, reserved, or transferring slot
    /// may remain. The Process detaches the matching sidecar under its lock and
    /// destroys both returned owners after releasing all namespace locks.
    pub(crate) fn take_empty_page(&mut self) -> Option<RetiredHandlePage> {
        if !matches!(self.lifecycle, TableLifecycle::Active) {
            return None;
        }
        let page = self.slots.empty_page()?;
        for index in self.slots.page_range(page) {
            if matches!(self.slots.get(index), Some(Slot::Vacant { .. })) {
                self.unlink_free_slot(index);
            }
        }
        Some(RetiredHandlePage {
            index: page,
            _page: self.slots.detach_empty(page),
        })
    }

    fn validate_reservation<const N: usize>(&self, token: &HandleReservation<N>) {
        if token.codec != self.codec {
            super::invariant_violation();
        }
        if !matches!(self.lifecycle, TableLifecycle::Active) {
            super::invariant_violation();
        }
        for position in 0..N {
            let index = token.slots[position];
            let generation = token.generations[position];
            if !matches!(
                self.slots.get(index),
                Some(Slot::Reserved {
                    generation: found_generation,
                    reservation,
                }) if *found_generation == generation && *reservation == token.reservation
            ) {
                super::invariant_violation();
            }
        }
    }

    fn abort_reservation<const N: usize>(&mut self, mut token: HandleReservation<N>) {
        self.validate_reservation(&token);
        for position in 0..N {
            let index = token.slots[position];
            let generation = token.generations[position];
            self.publish_vacant_slot(index, next_generation(generation));
        }
        token.completed = true;
    }

    fn publish_reservation<const N: usize>(
        &mut self,
        mut token: HandleReservation<N>,
        handles: [PreparedHandle; N],
    ) -> [HandleValue; N] {
        self.validate_reservation(&token);
        let values = token.values();
        for (position, handle) in handles.into_iter().enumerate() {
            let index = token.slots[position];
            let generation = token.generations[position];
            self.slots
                .replace(index, Slot::Occupied { generation, handle });
        }
        token.completed = true;
        values
    }

    #[cfg(test)]
    pub(crate) fn free_list_is_consistent_for_test(&self) -> bool {
        let mut current = self.free_head;
        let mut previous = None;
        let mut linked = 0usize;
        while let Some(index) = current {
            if linked >= self.slots.len() {
                return false;
            }
            let Some(Slot::Vacant {
                previous_free,
                next_free,
                ..
            }) = self.slots.get(index)
            else {
                return false;
            };
            if *previous_free != previous {
                return false;
            }
            previous = Some(index);
            current = *next_free;
            linked += 1;
        }
        linked == self.free_slots
            && self
                .slots
                .iter()
                .filter(|slot| matches!(slot, Slot::Vacant { .. }))
                .count()
                == self.free_slots
    }

    #[cfg(test)]
    pub(crate) const fn maximum_generation_for_test() -> u64 {
        GENERATION_LIMIT
    }

    #[cfg(test)]
    pub(crate) fn set_occupied_generation_for_test(
        &mut self,
        value: HandleValue,
        generation: u64,
    ) -> HandleValue {
        if generation == 0 || generation > GENERATION_LIMIT {
            super::invariant_violation();
        }
        let (index, expected) = self.codec.require_decoded(value);
        match self.slots.get_mut(index) {
            Some(Slot::Occupied {
                generation: found, ..
            }) if *found == expected => *found = generation,
            _ => super::invariant_violation(),
        }
        self.slots.record_generation(index, generation);
        self.codec.encode(index, generation)
    }

    pub(crate) fn get_info(&self, value: HandleValue) -> Result<HandleInfo, HandleError> {
        self.ensure_active()?;
        let handle = self.lookup(value)?;
        Ok(HandleInfo {
            koid: handle.object().koid(),
            kind: handle.object().kind(),
            rights: handle.rights.union(),
            flags: handle.flags,
        })
    }

    pub(crate) fn inspect_object(
        &self,
        value: HandleValue,
    ) -> Result<ErasedKernelRef<Diagnostic>, HandleError> {
        self.ensure_active()?;
        Ok(self.lookup(value)?.object().pin_diagnostic())
    }

    /// Copies a bounded page of object edges without exposing payload access.
    ///
    /// Every page is coherent under the owning Process lock. A complete scan
    /// is intentionally weakly consistent with concurrent handle mutation;
    /// generation-qualified values prevent a reused slot from aliasing an old
    /// edge. The slot budget also bounds lock hold time for sparse tables.
    pub(crate) fn scan_handles(
        &self,
        cursor: HandleScanCursor,
    ) -> Result<HandleSnapshotPage, HandleError> {
        self.ensure_active()?;
        let mut entries = [None; DIAGNOSTIC_PAGE_CAPACITY];
        let mut len = 0;
        let mut slot = cursor.next_slot.min(self.slots.len());
        let end = slot
            .saturating_add(DIAGNOSTIC_SLOT_BUDGET)
            .min(self.slots.len());
        while slot < end && len < DIAGNOSTIC_PAGE_CAPACITY {
            if let Some(Slot::Occupied { generation, handle }) = self.slots.get(slot) {
                entries[len] = Some(HandleSnapshot {
                    value: self.codec.encode(slot, *generation),
                    info: HandleInfo {
                        koid: handle.object().koid(),
                        kind: handle.object().kind(),
                        rights: handle.rights.union(),
                        flags: handle.flags,
                    },
                });
                len += 1;
            }
            slot += 1;
        }
        let next = (slot < self.slots.len()).then_some(HandleScanCursor { next_slot: slot });
        Ok(HandleSnapshotPage { entries, len, next })
    }

    /// Captures a receiver, its rights, and its generation under one table lock.
    /// The pin permits an admitted operation to finish after a concurrent close;
    /// it cannot duplicate receiver authority or replace handle-table transactions.
    pub(crate) fn resolve_invocation(
        &self,
        value: HandleValue,
    ) -> Result<InvocationReceiver, HandleError> {
        self.ensure_active()?;
        let handle = self.lookup(value)?;
        Ok(InvocationReceiver {
            handle: value,
            info: HandleInfo {
                koid: handle.object().koid(),
                kind: handle.object().kind(),
                rights: handle.rights.union(),
                flags: handle.flags,
            },
            object: handle.object().pin_erased(),
        })
    }

    /// Resolves authority and clones only an internal object-lifetime reference.
    ///
    /// Process serialization must remain held for this call and may be released
    /// immediately after it returns. A later close does not cancel the resolved
    /// operation and cannot invalidate its object reference.
    pub(crate) fn resolve<T: KernelObject>(
        &self,
        value: HandleValue,
        required: Rights,
    ) -> Result<ResolvedObject<T>, HandleError> {
        self.ensure_active()?;
        let handle = self.lookup(value)?;
        if !handle.rights.contains(required.decompose()) {
            return Err(HandleError::AccessDenied);
        }
        if handle.object().kind() != T::KIND {
            return Err(HandleError::WrongObjectType);
        }
        Ok(ResolvedObject {
            rights: handle.rights.union(),
            object: match handle.object().pin::<T>() {
                Some(object) => object,
                None => super::invariant_violation(),
            },
        })
    }

    /// Resolves one object exposing the common level-signal contract.
    pub(crate) fn resolve_waitable(
        &self,
        value: HandleValue,
        required: Rights,
    ) -> Result<ResolvedWaitable, HandleError> {
        self.ensure_active()?;
        let handle = self.lookup(value)?;
        if !handle.rights.contains(required.decompose()) {
            return Err(HandleError::AccessDenied);
        }
        let object = handle
            .object()
            .pin_waitable()
            .ok_or(HandleError::WrongObjectType)?;
        Ok(ResolvedWaitable { object })
    }

    /// Derives only a waitable identity from a Process supervisor. Ordinary
    /// Process duplication still requires DUPLICATE; this narrow operation
    /// cannot copy stop authority or derive another observer from an observer.
    pub(crate) fn derive_process_observer(
        &self,
        value: HandleValue,
    ) -> Result<PreparedHandle, HandleError> {
        self.ensure_active()?;
        let source = self.lookup(value)?;
        if source.object().kind() != ObjectKind::PROCESS {
            return Err(HandleError::WrongObjectType);
        }
        let rights = Rights::WAIT.union(Rights::TRANSFER);
        if !source
            .rights
            .contains(rights.union(Rights::REQUEST_STOP).decompose())
        {
            return Err(HandleError::AccessDenied);
        }
        source.try_duplicate(rights)
    }

    /// Prepares a duplicate with rights attenuated from the source handle.
    pub(crate) fn duplicate(
        &self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<PreparedHandle, HandleError> {
        self.ensure_active()?;
        let source = self.lookup(value)?;
        if !source
            .rights
            .propagation()
            .contains(PropagationRights::DUPLICATE)
        {
            return Err(HandleError::AccessDenied);
        }
        if !source.rights.contains(rights.decompose()) {
            return Err(HandleError::AccessDenied);
        }
        source.try_duplicate(rights)
    }

    /// Reversibly detaches a variable-size set of source handles.
    ///
    /// Validation is complete before the first slot changes. While the claim
    /// exists, an operation on an exact claimed value reports `Busy`; rollback
    /// restores every original value, while commit alone advances generations.
    #[cfg(test)]
    pub(crate) fn prepare_transfer(
        &mut self,
        requests: &[HandleTransferRequest],
        forbidden_object: Option<Koid>,
        forbidden_kind: Option<ObjectKind>,
        route: HandleTransferRoute,
    ) -> Result<HandleTransferClaim, HandleError> {
        let storage = HandleTransferStorage::try_new(requests.len())?;
        let mut storage = Some(storage);
        self.prepare_transfer_with_storage(
            requests,
            forbidden_object,
            forbidden_kind,
            route,
            &mut storage,
        )
    }

    pub(crate) fn prepare_transfer_with_storage(
        &mut self,
        requests: &[HandleTransferRequest],
        forbidden_object: Option<Koid>,
        forbidden_kind: Option<ObjectKind>,
        route: HandleTransferRoute,
        storage: &mut Option<HandleTransferStorage>,
    ) -> Result<HandleTransferClaim, HandleError> {
        self.ensure_active()?;
        validate_batch_count(requests.len())?;
        let prepared_storage = match storage.as_ref() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        if prepared_storage.count != requests.len()
            || !prepared_storage.entries.is_empty()
            || !prepared_storage.handles.is_empty()
        {
            super::invariant_violation();
        }

        let transfer = ReservationId::allocate()?;

        // This first pass performs every caller-controlled check without
        // changing the table. It also rejects aliasing source values, which
        // would otherwise make a later detach partially consume one slot.
        for (position, request) in requests.iter().enumerate() {
            if requests[..position]
                .iter()
                .any(|previous| previous.value == request.value)
            {
                return Err(HandleError::InvalidHandle);
            }
            let source = self.lookup(request.value)?;
            let offered_rights = request
                .offered_rights
                .unwrap_or_else(|| source.rights.union());
            let propagation = source.rights.propagation();
            let required_propagation = match request.operation {
                HandleTransferOperation::Move => PropagationRights::TRANSFER,
                HandleTransferOperation::Copy => PropagationRights::TRANSFER,
            };
            if !propagation.contains(required_propagation)
                || (request.operation == HandleTransferOperation::Copy
                    && !propagation.contains(PropagationRights::DUPLICATE))
                || !source.rights.contains(offered_rights.decompose())
                || !offered_rights
                    .decompose()
                    .contains(request.rights.decompose())
            {
                return Err(HandleError::AccessDenied);
            }
            // Transfer policy is based on the resolved object, not the caller's
            // type assertion. A mismatched `expected_kind` must not disguise a
            // kind which this transport cannot own safely.
            if forbidden_kind == Some(source.object().kind()) {
                return Err(HandleError::UnsupportedTransfer);
            }
            if forbidden_object == Some(source.object().koid()) {
                return Err(HandleError::AccessDenied);
            }
            if !route.permits(source.object().transfer_class()) {
                return Err(HandleError::UnsupportedTransfer);
            }
            if request
                .offered_kind
                .is_some_and(|kind| kind != source.object().kind())
                || request
                    .expected_kind
                    .is_some_and(|kind| kind != source.object().kind())
            {
                return Err(HandleError::WrongObjectType);
            }
        }

        // Acquire every fallible COPY owner before changing a source slot.
        // On failure, returning the storage through the caller-owned option
        // defers its destruction until after the Process lock is released.
        let mut transaction_storage = match storage.take() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        for request in requests {
            let prepared_copy = if request.operation == HandleTransferOperation::Copy {
                let source = match self.lookup(request.value) {
                    Ok(source) => source,
                    Err(_) => unreachable_handle_value(),
                };
                match source.try_duplicate(request.rights) {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        *storage = Some(transaction_storage);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            let (index, generation) = self.codec.require_decoded(request.value);
            transaction_storage.entries.push(TransferEntry {
                source: match request.operation {
                    HandleTransferOperation::Move => TransferSource::Move { index, generation },
                    HandleTransferOperation::Copy => TransferSource::Copy,
                },
                requested_rights: request.rights,
                prepared_copy,
            });
        }

        // Capacity and every fallible action are fixed above. Every exact slot
        // below is still protected by this table's exclusive caller, so source
        // detachment is an infallible ownership move.
        for position in 0..requests.len() {
            let request = match requests.get(position) {
                Some(request) => request,
                None => super::invariant_violation(),
            };
            let (index, generation) = self.codec.require_decoded(request.value);
            let handle = match request.operation {
                HandleTransferOperation::Move => {
                    let slot = self.slots.replace(index, Slot::Empty);
                    let Slot::Occupied { handle, .. } = slot else {
                        unreachable_handle_value();
                    };
                    self.slots.replace(
                        index,
                        Slot::TransferReserved {
                            generation,
                            transfer,
                        },
                    );
                    handle
                }
                HandleTransferOperation::Copy => {
                    match transaction_storage.entries.get_mut(position) {
                        Some(entry) => match entry.prepared_copy.take() {
                            Some(handle) => handle,
                            None => super::invariant_violation(),
                        },
                        None => super::invariant_violation(),
                    }
                }
            };
            transaction_storage.handles.push(handle);
        }
        self.active_transfers = match self.active_transfers.checked_add(1) {
            Some(count) => count,
            None => super::invariant_violation(),
        };

        Ok(HandleTransferClaim {
            codec: self.codec,
            transfer,
            entries: transaction_storage.entries,
            handles: Some(transaction_storage.handles),
            completed: false,
        })
    }

    /// Claims one handle for an object-specific consume-on-success operation.
    ///
    /// Consumption is not capability propagation: it requires the operation
    /// right supplied by the owning object protocol, but deliberately does not
    /// require `TRANSFER`. The exact kind and KOID bind a prior typed resolve
    /// to this table mutation so close-and-reuse cannot substitute an object.
    pub(crate) fn prepare_consumption_with_storage(
        &mut self,
        value: HandleValue,
        required: Rights,
        expected_kind: ObjectKind,
        expected_koid: Koid,
        storage: &mut Option<HandleTransferStorage>,
    ) -> Result<HandleTransferClaim, HandleError> {
        self.ensure_active()?;
        let prepared_storage = match storage.as_ref() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        if prepared_storage.count != 1
            || !prepared_storage.entries.is_empty()
            || !prepared_storage.handles.is_empty()
        {
            super::invariant_violation();
        }

        let source = self.lookup(value)?;
        if !source.rights.contains(required.decompose()) {
            return Err(HandleError::AccessDenied);
        }
        if source.object().kind() != expected_kind {
            return Err(HandleError::WrongObjectType);
        }
        if source.object().koid() != expected_koid {
            return Err(HandleError::InvalidHandle);
        }

        let transfer = ReservationId::allocate()?;
        let mut transaction_storage = match storage.take() {
            Some(storage) => storage,
            None => super::invariant_violation(),
        };
        let (index, generation) = self.codec.require_decoded(value);
        let slot = self.slots.replace(index, Slot::Empty);
        let Slot::Occupied { handle, .. } = slot else {
            unreachable_handle_value();
        };
        let retained_rights = handle.rights.union();
        self.slots.replace(
            index,
            Slot::TransferReserved {
                generation,
                transfer,
            },
        );
        transaction_storage.entries.push(TransferEntry {
            source: TransferSource::Move { index, generation },
            requested_rights: retained_rights,
            prepared_copy: None,
        });
        transaction_storage.handles.push(handle);
        self.active_transfers = match self.active_transfers.checked_add(1) {
            Some(count) => count,
            None => super::invariant_violation(),
        };

        Ok(HandleTransferClaim {
            codec: self.codec,
            transfer,
            entries: transaction_storage.entries,
            handles: Some(transaction_storage.handles),
            completed: false,
        })
    }

    fn validate_transfer(&self, claim: &HandleTransferClaim) {
        if claim.codec != self.codec {
            super::invariant_violation();
        }
        if !matches!(self.lifecycle, TableLifecycle::Active) {
            super::invariant_violation();
        }
        if self.active_transfers == 0 {
            super::invariant_violation();
        }
        if claim.handles.as_ref().map(Vec::len) != Some(claim.entries.len()) {
            super::invariant_violation();
        }
        for entry in &claim.entries {
            if let TransferSource::Move { index, generation } = entry.source
                && !matches!(
                    self.slots.get(index),
                    Some(Slot::TransferReserved {
                        generation: found_generation,
                        transfer,
                    }) if *found_generation == generation && *transfer == claim.transfer
                )
            {
                super::invariant_violation();
            }
        }
    }

    fn rollback_transfer(
        &mut self,
        mut claim: HandleTransferClaim,
    ) -> RetiredHandleTransferStorage {
        self.validate_transfer(&claim);
        let mut handles = match claim.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        };
        for position in (0..claim.entries.len()).rev() {
            let entry = match claim.entries.get(position) {
                Some(entry) => entry,
                None => super::invariant_violation(),
            };
            match entry.source {
                TransferSource::Move { index, generation } => {
                    if position >= handles.len() {
                        super::invariant_violation();
                    }
                    let handle = handles.swap_remove(position);
                    self.slots
                        .replace(index, Slot::Occupied { generation, handle });
                }
                // A copy owner was never installed in the table. It remains in
                // retired storage so its potential zero-active transition runs
                // only after the caller releases the table lock.
                TransferSource::Copy => {}
            }
        }
        self.active_transfers -= 1;
        claim.completed = true;
        RetiredHandleTransferStorage {
            _entries: core::mem::take(&mut claim.entries),
            _handles: handles,
        }
    }

    fn commit_transfer(
        &mut self,
        mut claim: HandleTransferClaim,
    ) -> (InTransitHandleBatch, RetiredHandleTransferStorage) {
        self.validate_transfer(&claim);
        let mut handles = match claim.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        };
        for (entry, handle) in claim.entries.iter().zip(handles.iter_mut()) {
            handle.rights = entry.requested_rights.decompose();
            if let TransferSource::Move { index, generation } = entry.source {
                self.publish_vacant_slot(index, next_generation(generation));
            }
        }
        self.active_transfers -= 1;
        claim.completed = true;
        (
            InTransitHandleBatch {
                handles: Some(handles),
            },
            RetiredHandleTransferStorage {
                _entries: core::mem::take(&mut claim.entries),
                _handles: Vec::new(),
            },
        )
    }

    /// Attenuates rights and advances the value in place, including generation
    /// wrap. Validation precedes mutation; the active owner never leaves the
    /// slot, and replacement needs neither allocation nor a second slot.
    pub(crate) fn replace(
        &mut self,
        value: HandleValue,
        rights: Rights,
    ) -> Result<HandleValue, HandleError> {
        self.ensure_active()?;
        let source = self.lookup(value)?;
        if !source.rights.contains(rights.decompose()) {
            return Err(HandleError::AccessDenied);
        }
        let (index, previous) = self.codec.require_decoded(value);
        let generation = next_generation(previous);
        match self.slots.get_mut(index) {
            Some(Slot::Occupied {
                generation: current,
                handle,
            }) => {
                *current = generation;
                handle.rights = rights.decompose();
            }
            _ => unreachable_handle_value(),
        };
        self.slots.record_generation(index, generation);
        Ok(self.codec.encode(index, generation))
    }

    /// Detaches one published handle and returns its slot for reuse.
    ///
    /// The returned owner remains active and must be completed only after the
    /// caller releases the Process handle-table lock.
    pub(crate) fn remove(&mut self, value: HandleValue) -> Result<ClosedHandle, HandleError> {
        self.ensure_active()?;
        self.remove_active(value)
    }

    fn remove_active(&mut self, value: HandleValue) -> Result<ClosedHandle, HandleError> {
        let (index, generation) = self.codec.decode(value)?;
        let slot = self
            .slots
            .get_mut(index)
            .ok_or(HandleError::InvalidHandle)?;
        if matches!(slot, Slot::TransferReserved { generation: found, .. } if *found == generation)
        {
            return Err(HandleError::Busy);
        }
        if !matches!(slot, Slot::Occupied { generation: found, .. } if *found == generation) {
            return Err(HandleError::InvalidHandle);
        }
        let removed = self.slots.replace(index, Slot::Empty);
        let Slot::Occupied { handle, .. } = removed else {
            unreachable_handle_value();
        };
        self.publish_vacant_slot(index, next_generation(generation));
        Ok(ClosedHandle {
            handle: Some(handle),
        })
    }

    /// Withdraws one ordinary owner after the Process closes admission. Active
    /// transfer/reservation tokens remain untouched and can roll back normally.
    /// The returned metadata carries no object reference or execution authority.
    pub(crate) fn remove_stopping_owner(
        &mut self,
        cursor: &mut usize,
    ) -> Option<(HandleSnapshot, ClosedHandle)> {
        if !matches!(self.lifecycle, TableLifecycle::Active) {
            return None;
        }
        let Some(index) = self.slots.next_occupied(*cursor) else {
            // A reserved transfer may roll back behind this cursor while the
            // Process drains. The next pass revisits that restored owner.
            *cursor = 0;
            return None;
        };
        *cursor = index + 1;
        let (generation, info) = match self.slots.get(index) {
            Some(Slot::Occupied { generation, handle }) => (
                *generation,
                HandleInfo {
                    koid: handle.object().koid(),
                    kind: handle.object().kind(),
                    rights: handle.rights.union(),
                    flags: handle.flags,
                },
            ),
            _ => unreachable_handle_value(),
        };
        let value = self.codec.encode(index, generation);
        match self.remove_active(value) {
            Ok(closed) => Some((HandleSnapshot { value, info }, closed)),
            Err(_) => unreachable_handle_value(),
        }
    }

    /// Starts exclusive Process teardown and blocks every new table operation.
    ///
    /// The Process lifecycle must prevent new syscall entry before calling this
    /// method and quiesce every detached slot reservation. The table rejects a
    /// premature transition rather than stranding an armed reservation token.
    /// Once teardown begins, lookup and publication remain blocked until the
    /// cursor has detached every owner and completed retirement.
    pub(crate) fn begin_teardown(&mut self) -> Result<TeardownCursor, HandleError> {
        self.ensure_active()?;
        if self.active_transfers != 0
            || self
                .slots
                .iter()
                .any(|slot| matches!(slot, Slot::Reserved { .. } | Slot::TransferReserved { .. }))
        {
            return Err(HandleError::OutstandingReservation);
        }
        let generation = self.next_teardown_generation;
        self.next_teardown_generation = self.next_teardown_generation.saturating_add(1);
        self.lifecycle = TableLifecycle::TearingDown { generation };
        Ok(TeardownCursor {
            generation,
            next_slot: 0,
            finished: false,
        })
    }

    /// Detaches the next owner in O(number of slots) total teardown work.
    ///
    /// The caller holds the Process table lock only for this method, releases
    /// it before `ClosedHandle::complete`, and then resumes with the same cursor.
    pub(crate) fn remove_next(&mut self, cursor: &mut TeardownCursor) -> Option<ClosedHandle> {
        if !matches!(
            self.lifecycle,
            TableLifecycle::TearingDown { generation } if generation == cursor.generation
        ) || cursor.finished
        {
            super::invariant_violation();
        }
        let index = self.slots.next_occupied(cursor.next_slot)?;
        cursor.next_slot = index + 1;
        let generation = match self.slots.get(index) {
            Some(Slot::Occupied { generation, .. }) => *generation,
            _ => unreachable_handle_value(),
        };
        match self.remove_active(self.codec.encode(index, generation)) {
            Ok(closed) => Some(closed),
            Err(_) => unreachable_handle_value(),
        }
    }

    /// Completes teardown after every detached owner was released out of lock.
    pub(crate) fn finish_teardown(&mut self, mut cursor: TeardownCursor) {
        if !matches!(
            self.lifecycle,
            TableLifecycle::TearingDown { generation } if generation == cursor.generation
        ) || cursor.finished
            || self.slots.iter().any(|slot| {
                matches!(
                    slot,
                    Slot::Reserved { .. } | Slot::TransferReserved { .. } | Slot::Occupied { .. }
                )
            })
        {
            super::invariant_violation();
        }
        self.lifecycle = TableLifecycle::Retired;
        cursor.finished = true;
    }

    /// Moves retired backing storage out for destruction without the table lock.
    pub(crate) fn take_retired_storage(&mut self) -> RetiredHandleStorage {
        if !matches!(self.lifecycle, TableLifecycle::Retired) {
            super::invariant_violation();
        }
        self.free_head = None;
        self.free_slots = 0;
        self.slots.take_retired()
    }

    fn ensure_active(&self) -> Result<(), HandleError> {
        if matches!(self.lifecycle, TableLifecycle::Active) {
            Ok(())
        } else {
            Err(HandleError::TableRetired)
        }
    }

    fn lookup(&self, value: HandleValue) -> Result<&PreparedHandle, HandleError> {
        let (index, generation) = self.codec.decode(value)?;
        match self.slots.get(index) {
            Some(Slot::Occupied {
                generation: found,
                handle,
            }) if *found == generation => Ok(handle),
            Some(Slot::TransferReserved {
                generation: found, ..
            }) if *found == generation => Err(HandleError::Busy),
            _ => Err(HandleError::InvalidHandle),
        }
    }
}

impl Drop for HandleTable {
    fn drop(&mut self) {
        if matches!(self.lifecycle, TableLifecycle::TearingDown { .. })
            || self.active_transfers != 0
            || self.slots.iter().any(|slot| {
                matches!(
                    slot,
                    Slot::Reserved { .. } | Slot::TransferReserved { .. } | Slot::Occupied { .. }
                )
            })
        {
            // Process teardown must detach and complete active owners through
            // remove_next instead of recursively dropping table contents.
            super::invariant_violation();
        }
    }
}

#[derive(Clone, Copy)]
enum TransferSource {
    Move { index: usize, generation: u64 },
    Copy,
}

struct TransferEntry {
    source: TransferSource,
    requested_rights: Rights,
    /// Temporary fallible COPY owner, consumed before the claim is exposed.
    prepared_copy: Option<PreparedHandle>,
}

/// Exact-count transfer backing prepared before Process locks are acquired.
///
/// Retaining the requested count prevents a later caller from pairing a small
/// allocation with a larger transaction and silently growing either Vec while
/// the handle table is locked.
pub(crate) struct HandleTransferStorage {
    count: usize,
    entries: Vec<TransferEntry>,
    handles: Vec<PreparedHandle>,
}

impl HandleTransferStorage {
    pub(crate) fn validate_count(count: usize) -> Result<(), HandleError> {
        validate_batch_count(count)
    }

    pub(crate) fn try_new(count: usize) -> Result<Self, HandleError> {
        Self::validate_count(count)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| HandleError::Allocation)?;
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(count)
            .map_err(|_| HandleError::Allocation)?;
        Ok(Self {
            count,
            entries,
            handles,
        })
    }
}

/// Reversible ownership of exact claimed source slots.
#[must_use = "commit or roll back the handle transfer claim"]
pub(crate) struct HandleTransferClaim {
    codec: HandleCodec,
    transfer: ReservationId,
    entries: Vec<TransferEntry>,
    handles: Option<Vec<PreparedHandle>>,
    completed: bool,
}

/// Empty transfer-token allocations returned for destruction outside locks.
pub(crate) struct RetiredHandleTransferStorage {
    _entries: Vec<TransferEntry>,
    _handles: Vec<PreparedHandle>,
}

impl HandleTransferClaim {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) const fn entry_allocation_size(count: usize) -> Option<usize> {
        count.checked_mul(core::mem::size_of::<TransferEntry>())
    }

    pub(crate) const fn handle_allocation_size(count: usize) -> Option<usize> {
        count.checked_mul(core::mem::size_of::<PreparedHandle>())
    }

    /// Source values consumed by commit.
    ///
    /// Copy dispositions deliberately do not participate in Process handle
    /// charge release because their source slots remain resident.
    pub(crate) fn values(&self) -> impl Iterator<Item = HandleValue> + '_ {
        self.entries.iter().filter_map(|entry| match entry.source {
            TransferSource::Move { index, generation } => {
                Some(self.codec.encode(index, generation))
            }
            TransferSource::Copy => None,
        })
    }

    #[cfg(test)]
    pub(crate) fn rollback(self, table: &mut HandleTable) {
        drop(table.rollback_transfer(self));
    }

    #[cfg(test)]
    pub(crate) fn commit(self, table: &mut HandleTable) -> InTransitHandleBatch {
        let (handles, retired) = table.commit_transfer(self);
        drop(retired);
        handles
    }

    pub(crate) fn rollback_with_storage(
        self,
        table: &mut HandleTable,
    ) -> RetiredHandleTransferStorage {
        table.rollback_transfer(self)
    }

    pub(crate) fn commit_with_storage(
        self,
        table: &mut HandleTable,
    ) -> (InTransitHandleBatch, RetiredHandleTransferStorage) {
        table.commit_transfer(self)
    }
}

impl Drop for HandleTransferClaim {
    fn drop(&mut self) {
        if !self.completed {
            super::invariant_violation();
        }
    }
}

/// Canonical Process handle-table lock acquisition order.
///
/// Process IDs are stable nonzero identities. A coordinator computes this
/// value before taking either Process lock, then acquires distinct tables in
/// the returned order. Equal IDs select the one-lock same-table path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleTableLockOrder {
    SameTable,
    SourceThenDestination,
    DestinationThenSource,
}

impl HandleTableLockOrder {
    pub(crate) fn for_processes(source_process: u64, destination_process: u64) -> Self {
        if source_process == 0 || destination_process == 0 {
            super::invariant_violation();
        }
        match source_process.cmp(&destination_process) {
            core::cmp::Ordering::Less => Self::SourceThenDestination,
            core::cmp::Ordering::Equal => Self::SameTable,
            core::cmp::Ordering::Greater => Self::DestinationThenSource,
        }
    }
}

/// A fully prepared direct source-to-destination capability commit.
///
/// Source claims and destination slots remain unpublished until the caller
/// holds the Process locks selected by `lock_order`. Production constructs this
/// owner only at the final, allocation-free commit point; earlier failures are
/// rolled back by the Process preparation owners. Host tests can also roll back
/// paired tables here. Returned storage must be dropped after releasing locks.
#[cfg_attr(not(test), must_use = "commit the direct handle transfer")]
#[cfg_attr(test, must_use = "commit or roll back the direct handle transfer")]
pub(crate) struct DirectHandleTransfer {
    source: Option<HandleTransferClaim>,
    destination: Option<HandleBatchReservation>,
    lock_order: HandleTableLockOrder,
}

/// Detached transaction storage safe to destroy after Process locks release.
pub(crate) struct RetiredDirectHandleTransfer {
    _source: RetiredHandleTransferStorage,
    _destination: RetiredHandleBatchReservationStorage,
}

impl DirectHandleTransfer {
    pub(crate) fn new(
        source_process: u64,
        destination_process: u64,
        source: HandleTransferClaim,
        destination: HandleBatchReservation,
    ) -> Self {
        if source.entries.len() != destination.values.len() {
            // Both tokens are private kernel products. Pairing reservations of
            // different sizes is an internal transaction-construction bug,
            // not a recoverable user request failure.
            super::invariant_violation();
        }
        let lock_order = HandleTableLockOrder::for_processes(source_process, destination_process);
        Self {
            source: Some(source),
            destination: Some(destination),
            lock_order,
        }
    }

    #[cfg(test)]
    pub(crate) const fn lock_order(&self) -> HandleTableLockOrder {
        self.lock_order
    }

    #[cfg(test)]
    pub(crate) fn destination_values(&self) -> &[HandleValue] {
        match self.destination.as_ref() {
            Some(destination) => destination.values(),
            None => super::invariant_violation(),
        }
    }

    pub(crate) fn commit_between(
        mut self,
        source_table: &mut HandleTable,
        destination_table: &mut HandleTable,
    ) -> RetiredDirectHandleTransfer {
        if self.lock_order == HandleTableLockOrder::SameTable {
            super::invariant_violation();
        }
        self.commit_locked(source_table, destination_table)
    }

    pub(crate) fn commit_within(mut self, table: &mut HandleTable) -> RetiredDirectHandleTransfer {
        if self.lock_order != HandleTableLockOrder::SameTable {
            super::invariant_violation();
        }
        let source = self.take_source();
        let destination = self.take_destination();
        let (handles, source_storage) = table.commit_transfer(source);
        let destination_storage = destination.publish(table, handles.into_prepared_handles());
        RetiredDirectHandleTransfer {
            _source: source_storage,
            _destination: destination_storage,
        }
    }

    #[cfg(test)]
    pub(crate) fn rollback_between(
        mut self,
        source_table: &mut HandleTable,
        destination_table: &mut HandleTable,
    ) -> RetiredDirectHandleTransfer {
        if self.lock_order == HandleTableLockOrder::SameTable {
            super::invariant_violation();
        }
        self.rollback_locked(source_table, destination_table)
    }

    fn commit_locked(
        &mut self,
        source_table: &mut HandleTable,
        destination_table: &mut HandleTable,
    ) -> RetiredDirectHandleTransfer {
        let source = self.take_source();
        let destination = self.take_destination();
        let (handles, source_storage) = source_table.commit_transfer(source);
        let destination_storage =
            destination.publish(destination_table, handles.into_prepared_handles());
        RetiredDirectHandleTransfer {
            _source: source_storage,
            _destination: destination_storage,
        }
    }

    #[cfg(test)]
    fn rollback_locked(
        &mut self,
        source_table: &mut HandleTable,
        destination_table: &mut HandleTable,
    ) -> RetiredDirectHandleTransfer {
        let source_storage = source_table.rollback_transfer(self.take_source());
        let destination_storage = self.take_destination().abort(destination_table);
        RetiredDirectHandleTransfer {
            _source: source_storage,
            _destination: destination_storage,
        }
    }

    fn take_source(&mut self) -> HandleTransferClaim {
        match self.source.take() {
            Some(source) => source,
            None => super::invariant_violation(),
        }
    }

    fn take_destination(&mut self) -> HandleBatchReservation {
        match self.destination.take() {
            Some(destination) => destination,
            None => super::invariant_violation(),
        }
    }
}

impl Drop for DirectHandleTransfer {
    fn drop(&mut self) {
        if self.source.is_some() || self.destination.is_some() {
            super::invariant_violation();
        }
    }
}

/// Active capability owners detached from every process-local namespace.
#[must_use = "publish or explicitly release the in-transit handles"]
pub(crate) struct InTransitHandleBatch {
    handles: Option<Vec<PreparedHandle>>,
}

impl InTransitHandleBatch {
    #[cfg(test)]
    pub(crate) fn from_prepared_handles(handles: Vec<PreparedHandle>) -> Self {
        if handles.is_empty() {
            super::invariant_violation();
        }
        Self {
            handles: Some(handles),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.handles.as_ref() {
            Some(handles) => handles.len(),
            None => super::invariant_violation(),
        }
    }

    pub(crate) fn into_prepared_handles(mut self) -> Vec<PreparedHandle> {
        match self.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        }
    }

    #[cfg(test)]
    pub(crate) fn release(self) {
        let mut retirement = ObjectRetirement::new();
        self.release_into(&mut retirement);
        retirement.drain();
    }

    pub(crate) fn release_into(mut self, retirement: &mut ObjectRetirement) {
        let handles = match self.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        };
        for mut handle in handles {
            handle.release_into(retirement);
        }
    }
}

impl Drop for InTransitHandleBatch {
    fn drop(&mut self) {
        if self.handles.is_some() {
            super::invariant_violation();
        }
    }
}

/// Monotonic progress token for allocation-free Process handle teardown.
#[must_use = "finish Process handle-table teardown"]
pub(crate) struct TeardownCursor {
    generation: u64,
    next_slot: usize,
    finished: bool,
}

impl Drop for TeardownCursor {
    fn drop(&mut self) {
        if !self.finished {
            super::invariant_violation();
        }
    }
}

/// An active handle detached from its Process table.
///
/// `complete` may invoke the object's zero-active callback and therefore must
/// execute after releasing the Process handle-table lock. Dropping an armed
/// owner is an invariant violation rather than an implicit callback site.
#[must_use = "complete removal after releasing the Process handle-table lock"]
pub(crate) struct ClosedHandle {
    handle: Option<PreparedHandle>,
}

impl ClosedHandle {
    pub(crate) fn complete(mut self) {
        let handle = self.handle.take();
        drop(handle);
    }
}

impl Drop for ClosedHandle {
    fn drop(&mut self) {
        if self.handle.is_some() {
            super::invariant_violation();
        }
    }
}

struct ReservedSlot {
    index: usize,
    generation: u64,
}

/// Exact-count runtime reservation storage allocated outside Process locks.
pub(crate) struct HandleBatchReservationStorage {
    count: usize,
    slots: Vec<ReservedSlot>,
    values: Vec<HandleValue>,
}

impl HandleBatchReservationStorage {
    pub(crate) const fn maximum_count() -> usize {
        MAX_RESERVATION_SLOTS
    }

    pub(crate) fn validate_count(count: usize) -> Result<(), HandleError> {
        validate_batch_count(count)
    }

    pub(crate) const fn allocation_size(count: usize) -> Option<usize> {
        let slots = match count.checked_mul(core::mem::size_of::<ReservedSlot>()) {
            Some(bytes) => bytes,
            None => return None,
        };
        let values = match count.checked_mul(core::mem::size_of::<HandleValue>()) {
            Some(bytes) => bytes,
            None => return None,
        };
        slots.checked_add(values)
    }

    pub(crate) fn try_new(count: usize) -> Result<Self, HandleError> {
        Self::validate_count(count)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(count)
            .map_err(|_| HandleError::Allocation)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| HandleError::Allocation)?;
        Ok(Self {
            count,
            slots,
            values,
        })
    }
}

/// Runtime-sized unpublished destination slots for IPC receive.
#[must_use = "publish or abort the handle batch reservation"]
pub(crate) struct HandleBatchReservation {
    reservation: ReservationId,
    slots: Vec<ReservedSlot>,
    values: Vec<HandleValue>,
    completed: bool,
}

/// Empty batch-token backing returned for destruction after releasing locks.
pub(crate) struct RetiredHandleBatchReservationStorage {
    _slots: Vec<ReservedSlot>,
    _values: Vec<HandleValue>,
    _handles: Vec<PreparedHandle>,
}

#[cfg(test)]
impl RetiredHandleBatchReservationStorage {
    pub(crate) fn retained_handle_capacity_for_test(&self) -> usize {
        self._handles.capacity()
    }
}

impl HandleBatchReservation {
    pub(crate) fn values(&self) -> &[HandleValue] {
        &self.values
    }

    /// Retains the prefix needed by a matched rendezvous and releases every
    /// unused tail slot without allocating.
    pub(crate) fn trim_to(&mut self, table: &mut HandleTable, count: usize) {
        if count == 0 || count > self.slots.len() || self.completed {
            super::invariant_violation();
        }
        for slot in &self.slots {
            if !matches!(
                table.slots.get(slot.index),
                Some(Slot::Reserved { generation, reservation })
                    if *generation == slot.generation && *reservation == self.reservation
            ) {
                super::invariant_violation();
            }
        }

        for slot in &self.slots[count..] {
            table.publish_vacant_slot(slot.index, next_generation(slot.generation));
        }
        self.slots.truncate(count);
        self.values.truncate(count);
    }

    pub(crate) fn publish(
        mut self,
        table: &mut HandleTable,
        mut handles: Vec<PreparedHandle>,
    ) -> RetiredHandleBatchReservationStorage {
        if handles.len() != self.slots.len() || !matches!(table.lifecycle, TableLifecycle::Active) {
            super::invariant_violation();
        }
        for slot in &self.slots {
            if !matches!(
                table.slots.get(slot.index),
                Some(Slot::Reserved { generation, reservation })
                    if *generation == slot.generation && *reservation == self.reservation
            ) {
                super::invariant_violation();
            }
        }
        // Move owners out in reverse order without consuming the Vec itself.
        // Its now-empty allocation is returned to the caller for destruction
        // after releasing the Process lock.
        for slot in self.slots.iter().rev() {
            let handle = match handles.pop() {
                Some(handle) => handle,
                None => super::invariant_violation(),
            };
            table.slots.replace(
                slot.index,
                Slot::Occupied {
                    generation: slot.generation,
                    handle,
                },
            );
        }
        self.completed = true;
        RetiredHandleBatchReservationStorage {
            _slots: core::mem::take(&mut self.slots),
            _values: core::mem::take(&mut self.values),
            _handles: handles,
        }
    }

    /// Publishes this reservation from the tail of one preallocated batch.
    ///
    /// Callers reverse the complete source batch once, then commit consecutive
    /// reservations in value order without allocating per-reservation Vecs.
    /// The source Vec allocation remains caller-owned for destruction after
    /// the table lock is released.
    pub(crate) fn publish_from_reversed(
        mut self,
        table: &mut HandleTable,
        handles: &mut Vec<PreparedHandle>,
    ) -> RetiredHandleBatchReservationStorage {
        if handles.len() < self.slots.len() || !matches!(table.lifecycle, TableLifecycle::Active) {
            super::invariant_violation();
        }
        for slot in &self.slots {
            if !matches!(
                table.slots.get(slot.index),
                Some(Slot::Reserved { generation, reservation })
                    if *generation == slot.generation && *reservation == self.reservation
            ) {
                super::invariant_violation();
            }
        }
        for slot in &self.slots {
            let handle = match handles.pop() {
                Some(handle) => handle,
                None => super::invariant_violation(),
            };
            table.slots.replace(
                slot.index,
                Slot::Occupied {
                    generation: slot.generation,
                    handle,
                },
            );
        }
        self.completed = true;
        RetiredHandleBatchReservationStorage {
            _slots: core::mem::take(&mut self.slots),
            _values: core::mem::take(&mut self.values),
            _handles: Vec::new(),
        }
    }

    pub(crate) fn abort(mut self, table: &mut HandleTable) -> RetiredHandleBatchReservationStorage {
        if !matches!(table.lifecycle, TableLifecycle::Active) {
            super::invariant_violation();
        }
        for slot in &self.slots {
            if !matches!(
                table.slots.get(slot.index),
                Some(Slot::Reserved { generation, reservation })
                    if *generation == slot.generation && *reservation == self.reservation
            ) {
                super::invariant_violation();
            }
            table.publish_vacant_slot(slot.index, next_generation(slot.generation));
        }
        self.completed = true;
        RetiredHandleBatchReservationStorage {
            _slots: core::mem::take(&mut self.slots),
            _values: core::mem::take(&mut self.values),
            _handles: Vec::new(),
        }
    }
}

impl Drop for HandleBatchReservation {
    fn drop(&mut self) {
        if !self.completed {
            super::invariant_violation();
        }
    }
}

/// Linear ownership of slots which cannot yet be resolved by lookup.
///
/// The token deliberately does not borrow the table. A Process may release its
/// table lock while a pinned user-write reservation copies the future numeric
/// values, then reacquire the same table and perform one infallible publish.
/// Every exit path must explicitly publish or abort the token.
#[must_use = "publish or abort the handle-slot reservation"]
pub(crate) struct HandleReservation<const N: usize> {
    codec: HandleCodec,
    reservation: ReservationId,
    slots: [usize; N],
    generations: [u64; N],
    completed: bool,
}

impl<const N: usize> HandleReservation<N> {
    /// Returns the future values while every corresponding slot is unresolved.
    pub(crate) fn values(&self) -> [HandleValue; N] {
        array::from_fn(|position| {
            self.codec
                .encode(self.slots[position], self.generations[position])
        })
    }

    /// Publishes all active handles after reacquiring the owning table lock.
    pub(crate) fn publish(
        self,
        table: &mut HandleTable,
        handles: [PreparedHandle; N],
    ) -> [HandleValue; N] {
        table.publish_reservation(self, handles)
    }

    /// Invalidates every future value after reacquiring the owning table lock.
    pub(crate) fn abort(self, table: &mut HandleTable) {
        table.abort_reservation(self);
    }
}

impl<const N: usize> Drop for HandleReservation<N> {
    fn drop(&mut self) {
        if !self.completed {
            super::invariant_violation();
        }
    }
}

/// One invocation's receiver authority. Only handle-table resolution constructs it.
/// Dynamic rights checks and downcasts use this captured authority; normal object
/// operations never look up the receiver handle again. Namespace mutations still
/// validate the original generation and, for consuming transactions, the KOID.
pub(crate) struct InvocationReceiver {
    handle: HandleValue,
    info: HandleInfo,
    object: ErasedKernelRef<OperationPin>,
}

impl InvocationReceiver {
    pub(crate) const fn handle(&self) -> HandleValue {
        self.handle
    }
    pub(crate) const fn info(&self) -> HandleInfo {
        self.info
    }
    pub(crate) const fn kind(&self) -> ObjectKind {
        self.info.kind
    }

    pub(crate) fn require(&self, rights: Rights) -> Result<(), HandleError> {
        if self.info.rights.contains(rights) {
            Ok(())
        } else {
            Err(HandleError::AccessDenied)
        }
    }

    pub(crate) fn resolve<T: KernelObject>(
        &self,
        rights: Rights,
    ) -> Result<ResolvedObject<T>, HandleError> {
        self.require(rights)?;
        let object = self.object.pin::<T>().ok_or(HandleError::WrongObjectType)?;
        Ok(ResolvedObject {
            object,
            rights: self.info.rights,
        })
    }

    pub(crate) fn resolve_waitable(&self, rights: Rights) -> Result<ResolvedWaitable, HandleError> {
        self.require(rights)?;
        self.object
            .signal_source()
            .ok_or(HandleError::WrongObjectType)?;
        Ok(ResolvedWaitable {
            object: self.object.clone(),
        })
    }
}

/// Type-checked internal object reference retained beyond the handle-table lock.
pub(crate) struct ResolvedObject<T: KernelObject> {
    object: crate::object::KernelRef<T, OperationPin>,
    rights: Rights,
}

/// Type-erased wait authority retained beyond the handle-table lock.
pub(crate) struct ResolvedWaitable {
    object: ErasedKernelRef<OperationPin>,
}

impl ResolvedWaitable {
    pub(crate) fn kind(&self) -> ObjectKind {
        self.object.kind()
    }
    pub(crate) fn source(&self) -> SignalSource<'_> {
        match self.object.signal_source() {
            Some(source) => source,
            None => super::invariant_violation(),
        }
    }

    #[cfg(test)]
    pub(crate) fn koid(&self) -> Koid {
        self.object.koid()
    }
}

impl<T: KernelObject> ResolvedObject<T> {
    /// Source grant captured atomically with authority resolution. Derivations
    /// use this snapshot without reopening a possibly closed or reused handle.
    pub(crate) fn rights(&self) -> Rights {
        self.rights
    }

    /// Returns the compiler-checked payload reference.
    ///
    /// `HandleTable::resolve` checked type coherence before constructing this
    /// immutable typed kernel reference, so access needs no repeated downcast.
    pub(crate) fn object(&self) -> &T {
        self.object.object()
    }

    pub(crate) fn koid(&self) -> Koid {
        self.object.koid()
    }

    /// Retains the canonical erased owner after typed authority validation.
    pub(crate) fn into_operation_pin(self) -> crate::object::KernelRef<T, OperationPin> {
        self.object
    }
}
