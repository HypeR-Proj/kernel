// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-facing IPC transactions and user-memory commit ordering.

use crate::capability::handle_wire;

use crate::capability::InvocationReceiver;

use alloc::vec::Vec;

use crate::authority::Rights;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceKind,
};
use {
    crate::capability::HandleError, crate::capability::HandleTransferOperation,
    crate::capability::HandleTransferRequest, crate::capability::HandleTransferRoute,
    crate::capability::HandleValue, crate::capability::ResolvedObject,
};
use {crate::mm::user_space::UserSlice, crate::mm::user_space::UserWriteReservation};
use {crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectWaitError};
use {
    crate::process::PreparedDirectProcessHandleTransfer, crate::process::Process,
    crate::process::ProcessError, crate::process::ProcessHandleBatchReservation,
};

use super::capability_wire::{self, Operation, RightsOffer};
use super::{
    ByteChannel, ByteChannelError, CapabilityChannel, CapabilityChannelError,
    CapabilityDeliveryInfo, CapabilityReceiveContract, CapabilityReceiveOutcome,
    CapabilitySlotContract, PreparedByteMessage,
};

const CAPABILITY_RECORD_BYTES: usize = capability_wire::RECORD_BYTES;
const MAX_CAPABILITY_BYTES: usize =
    hyper::abi::native::HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_MESSAGE_BYTES as usize;
const MAX_CAPABILITIES: usize =
    hyper::abi::native::HYPER_NATIVE_CAPABILITY_CHANNEL_MAX_HANDLES as usize;
const MAX_CAPABILITY_RECORD_BUFFER: usize = CAPABILITY_RECORD_BYTES * MAX_CAPABILITIES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ByteChannelReadOutcome {
    Received { bytes: u64 },
    BufferTooSmall { bytes: u64 },
}

#[derive(Debug)]
pub(crate) enum ByteChannelServiceError {
    InvalidBuffer,
    Process(ProcessError),
    Channel(ByteChannelError),
}

impl From<ProcessError> for ByteChannelServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<ByteChannelError> for ByteChannelServiceError {
    fn from(error: ByteChannelError) -> Self {
        Self::Channel(error)
    }
}

#[derive(Debug)]
pub(crate) enum CapabilityChannelServiceError {
    InvalidInput,
    Process(ProcessError),
    Channel(CapabilityChannelError),
    Wait(ObjectWaitError),
}

impl From<ProcessError> for CapabilityChannelServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<CapabilityChannelError> for CapabilityChannelServiceError {
    fn from(error: CapabilityChannelError) -> Self {
        Self::Channel(error)
    }
}

impl From<ObjectWaitError> for CapabilityChannelServiceError {
    fn from(error: ObjectWaitError) -> Self {
        Self::Wait(error)
    }
}

#[derive(Clone, Copy)]
struct CapabilitySendDisposition {
    value: HandleValue,
    offered_rights: Option<Rights>,
    offered_kind: ObjectKind,
    operation: HandleTransferOperation,
}

struct PreparedCapabilitySend {
    bytes: Vec<u8>,
    dispositions: Vec<CapabilitySendDisposition>,
    requests: Vec<HandleTransferRequest>,
    _charge: CommittedCharge,
}

/// Receiver-owned resources established before FIFO visibility.
pub(super) struct CapabilityReceiveTarget {
    process: Process,
    byte_output: Option<UserWriteReservation>,
    slot_output: Option<UserWriteReservation>,
    handles: Option<ProcessHandleBatchReservation>,
    slot_bytes: Vec<u8>,
    _slot_charge: Option<CommittedCharge>,
}

pub(crate) fn capability_channel_create(
    process: &Process,
) -> Result<[HandleValue; 2], CapabilityChannelServiceError> {
    let (first, second) = CapabilityChannel::try_pair(&process.resource_domain())?;
    Ok(process.create_object_pair(
        first,
        second,
        <CapabilityChannel as KernelObject>::SUPPORTED_RIGHTS,
    )?)
}

/// Attempts one synchronous send after copying and validating all user input.
///
/// No source handle is claimed until the oldest receiver is selected. A
/// rejected send therefore leaves every MOVE source unchanged, and a selected
/// receiver observes the same precommit error as the sender.
pub(crate) fn capability_channel_try_send(
    process: &Process,
    endpoint_value: &InvocationReceiver,
    bytes: Option<UserSlice>,
    disposition_records: Option<UserSlice>,
) -> Result<(), CapabilityChannelServiceError> {
    let endpoint = endpoint_value
        .resolve::<CapabilityChannel>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let mut prepared = PreparedCapabilitySend::copy_from_user(process, bytes, disposition_records)?;
    send_prepared_capabilities(process, &endpoint, &mut prepared)
}

// Receiver matching/publication retains large rollback owners. Keep this frame
// out of sender input admission and its usercopy/COW/allocator call chain. The
// wrapper retains the authorized endpoint and copied input throughout delivery.
#[inline(never)]
fn send_prepared_capabilities(
    process: &Process,
    endpoint: &ResolvedObject<CapabilityChannel>,
    prepared: &mut PreparedCapabilitySend,
) -> Result<(), CapabilityChannelServiceError> {
    let claim = endpoint
        .object()
        .try_match(prepared.bytes.len(), prepared.dispositions.len())?;
    let mut target = match claim.take_target() {
        Some(target) => target,
        None => {
            let error = CapabilityChannelError::Internal;
            claim.reject(error);
            return Err(error.into());
        }
    };

    if let Err(error) = claim.capacity_result() {
        target.abort();
        claim.reject(error);
        return Err(error.into());
    }

    target.narrow_handles(prepared.dispositions.len());
    let contract = claim.contract();
    for (source, destination) in prepared.dispositions.iter().zip(contract.slots().iter()) {
        prepared.requests.push(HandleTransferRequest {
            value: source.value,
            offered_rights: source.offered_rights,
            rights: destination.rights,
            offered_kind: Some(source.offered_kind),
            expected_kind: destination.expected_kind,
            operation: source.operation,
        });
    }

    let mut direct = if prepared.requests.is_empty() {
        None
    } else {
        let source = match process.prepare_handle_transfer(
            &prepared.requests,
            Some(endpoint.koid()),
            None,
            HandleTransferRoute::Rendezvous,
        ) {
            Ok(source) => source,
            Err(error) => {
                let channel_error = channel_error_from_process(error);
                target.abort();
                claim.reject(channel_error);
                return Err(channel_error.into());
            }
        };
        let destination = match target.take_handles() {
            Some(destination) => destination,
            None => capability_service_invariant("nonempty transfer has no destination slots"),
        };
        Some(PreparedDirectProcessHandleTransfer::new(
            source,
            target.process.clone(),
            destination,
        ))
    };

    let destination_values = direct.as_ref().map_or(
        &[][..],
        PreparedDirectProcessHandleTransfer::destination_values,
    );
    if let Err(error) = target.write_outputs(
        &prepared.bytes,
        destination_values,
        &contract.slots()[..prepared.dispositions.len()],
    ) {
        if let Some(transfer) = direct.take() {
            transfer.rollback();
        }
        target.finish_output_access();
        let channel_error = CapabilityChannelError::UserMemoryFault;
        claim.reject(channel_error);
        let _ = error;
        return Err(channel_error.into());
    }

    if let Some(transfer) = direct.take()
        && let Err(failure) = transfer.commit()
    {
        failure.transfer.rollback();
        target.finish_output_access();
        // Output may already contain a complete prefix, so the ABI requires
        // FAULT even when the late admission loss originated elsewhere.
        let error = CapabilityChannelError::UserMemoryFault;
        claim.reject(error);
        return Err(error.into());
    }

    target.finish_output_access();
    claim.commit(CapabilityDeliveryInfo {
        bytes: prepared.bytes.len(),
        handles: prepared.dispositions.len(),
    });
    Ok(())
}

/// Publishes a fully pinned typed receiver and waits for one sender.
pub(crate) fn capability_channel_receive(
    process: &Process,
    endpoint_value: &InvocationReceiver,
    deadline_nanoseconds: u64,
    byte_output: Option<UserSlice>,
    slot_records: Option<UserSlice>,
    cancellation_requested: impl FnOnce() -> bool,
) -> Result<CapabilityReceiveOutcome, CapabilityChannelServiceError> {
    let endpoint = endpoint_value
        .resolve::<CapabilityChannel>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    if byte_output
        .zip(slot_records)
        .is_some_and(|(bytes, slots)| !bytes.is_disjoint(slots))
    {
        return Err(CapabilityChannelServiceError::InvalidInput);
    }
    let byte_capacity = slice_length(byte_output)?;
    if byte_capacity > MAX_CAPABILITY_BYTES {
        return Err(CapabilityChannelServiceError::InvalidInput);
    }
    let (contract, decode_charge) = copy_receive_contract(process, byte_capacity, slot_records)?;
    let slot_count = contract.slot_count();
    let prepared = endpoint
        .object()
        .prepare_receive(&process.resource_domain(), contract)?;
    // The persistent registration now owns its vector's accounting. Retain
    // the decode charge until here so preemption cannot leave it uncharged.
    drop(decode_charge);
    let mut target = CapabilityReceiveTarget::new(process);
    if let Err(error) = target.prepare(byte_output, slot_records, slot_count) {
        target.abort();
        return Err(error);
    }
    Ok(prepared.attach_target(target).wait(
        &process.resource_domain(),
        deadline_nanoseconds,
        cancellation_requested,
    )?)
}

pub(crate) fn byte_channel_create(
    process: &Process,
) -> Result<[HandleValue; 2], ByteChannelServiceError> {
    let (first, second) = ByteChannel::try_pair(&process.resource_domain())?;
    Ok(process.create_object_pair(
        first,
        second,
        <ByteChannel as KernelObject>::SUPPORTED_RIGHTS,
    )?)
}

/// Copies and commits one byte message.
pub(crate) fn byte_channel_write(
    process: &Process,
    endpoint_value: &InvocationReceiver,
    bytes: Option<UserSlice>,
) -> Result<(), ByteChannelServiceError> {
    let endpoint = endpoint_value
        .resolve::<ByteChannel>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let byte_count = bytes
        .map(|range| usize::try_from(range.length()))
        .transpose()
        .map_err(|_| ByteChannelServiceError::InvalidBuffer)?
        .unwrap_or(0);
    let mut message = PreparedByteMessage::try_new(&process.resource_domain(), byte_count)?;
    if let Some(source) = bytes {
        process.copy_from_user(source, message.bytes_mut())?;
    }
    let write = endpoint.object().prepare_write(&message)?;
    write.publish(message);
    Ok(())
}

/// Receives one FIFO byte message after validating its exact output range.
pub(crate) fn byte_channel_read(
    process: &Process,
    endpoint_value: &InvocationReceiver,
    output: Option<UserSlice>,
) -> Result<ByteChannelReadOutcome, ByteChannelServiceError> {
    let endpoint = endpoint_value
        .resolve::<ByteChannel>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    let info = endpoint.object().peek()?;
    let required =
        u64::try_from(info.bytes()).map_err(|_| ByteChannelServiceError::InvalidBuffer)?;
    if output.map_or(0, UserSlice::length) < required {
        return Ok(ByteChannelReadOutcome::BufferTooSmall { bytes: required });
    }

    let claim = endpoint.object().claim(info)?;
    let mut write = match reserve_exact_output(process, output, required) {
        Ok(write) => write,
        Err(error) => {
            claim.abort();
            return Err(error.into());
        }
    };
    let copy_error = write
        .as_ref()
        .and_then(|output| output.copy_from(claim.bytes()).err());
    if let Some(error) = copy_error {
        drop(write.take());
        claim.abort();
        return Err(ProcessError::UserMemory(error).into());
    }

    let received = claim.commit();
    received.release();
    if let Some(write) = write {
        write.complete();
    }
    Ok(ByteChannelReadOutcome::Received { bytes: required })
}

fn reserve_exact_output(
    process: &Process,
    capacity: Option<UserSlice>,
    length: u64,
) -> Result<Option<UserWriteReservation>, ProcessError> {
    if length == 0 {
        return Ok(None);
    }
    let capacity = match capacity {
        Some(capacity) => capacity,
        None => byte_channel_service_invariant("validated nonempty output has no buffer"),
    };
    let range = UserSlice::new(capacity.base(), length)
        .map_err(|error| ProcessError::UserMemory(error.into()))?;
    Ok(Some(process.reserve_user_write(range)?))
}

impl PreparedCapabilitySend {
    // Input decoding and user-memory admission have their own stack scratch.
    // Release it before matching/committing the receiver and waking Threads.
    #[inline(never)]
    fn copy_from_user(
        process: &Process,
        bytes: Option<UserSlice>,
        records: Option<UserSlice>,
    ) -> Result<Self, CapabilityChannelServiceError> {
        let byte_count = slice_length(bytes)?;
        let record_bytes = slice_length(records)?;
        if byte_count > MAX_CAPABILITY_BYTES
            || record_bytes > MAX_CAPABILITY_RECORD_BUFFER
            || record_bytes % CAPABILITY_RECORD_BYTES != 0
        {
            return Err(CapabilityChannelServiceError::InvalidInput);
        }
        let disposition_count = record_bytes / CAPABILITY_RECORD_BYTES;
        let request_bytes = disposition_count
            .checked_mul(
                core::mem::size_of::<CapabilitySendDisposition>()
                    .saturating_add(core::mem::size_of::<HandleTransferRequest>()),
            )
            .ok_or(CapabilityChannelServiceError::InvalidInput)?;
        let charged_bytes = byte_count
            .checked_add(request_bytes)
            .and_then(|bytes| bytes.checked_add(record_bytes))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(CapabilityChannelServiceError::InvalidInput)?;
        let charge = process
            .resource_domain()
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, charged_bytes)
                    .with(ResourceKind::IpcMessages, 1)
                    .with(
                        ResourceKind::IpcBytes,
                        u64::try_from(byte_count)
                            .map_err(|_| CapabilityChannelServiceError::InvalidInput)?,
                    ),
            )
            .map_err(ProcessError::from)?
            .commit();

        let mut owned_bytes = Vec::new();
        owned_bytes.try_reserve_exact(byte_count).map_err(|_| {
            CapabilityChannelServiceError::Channel(CapabilityChannelError::Allocation)
        })?;
        owned_bytes.resize(byte_count, 0);
        if let (Some(source), false) = (bytes, owned_bytes.is_empty()) {
            process.copy_from_user(source, &mut owned_bytes)?;
        }

        let mut raw = try_zeroed_bytes(record_bytes)?;
        if let (Some(source), false) = (records, record_bytes == 0) {
            let destination = raw
                .get_mut(..record_bytes)
                .ok_or(CapabilityChannelServiceError::InvalidInput)?;
            process.copy_from_user(source, destination)?;
        }
        let mut dispositions = Vec::new();
        dispositions
            .try_reserve_exact(disposition_count)
            .map_err(|_| {
                CapabilityChannelServiceError::Channel(CapabilityChannelError::Allocation)
            })?;
        for record in raw[..record_bytes].chunks_exact(CAPABILITY_RECORD_BYTES) {
            let disposition = decode_send_disposition(record)?;
            if dispositions
                .iter()
                .any(|present: &CapabilitySendDisposition| present.value == disposition.value)
            {
                return Err(CapabilityChannelServiceError::InvalidInput);
            }
            dispositions.push(disposition);
        }
        let mut requests = Vec::new();
        requests.try_reserve_exact(disposition_count).map_err(|_| {
            CapabilityChannelServiceError::Channel(CapabilityChannelError::Allocation)
        })?;
        Ok(Self {
            bytes: owned_bytes,
            dispositions,
            requests,
            _charge: charge,
        })
    }
}

impl CapabilityReceiveTarget {
    fn new(process: &Process) -> Self {
        Self {
            process: process.clone(),
            byte_output: None,
            slot_output: None,
            handles: None,
            slot_bytes: Vec::new(),
            _slot_charge: None,
        }
    }

    // Fill the single caller-owned target in place. Returning another full
    // target keeps duplicate output plans live below COW and scheduler frames.
    // The unpublished caller must abort this target on every preparation error.
    #[inline(never)]
    fn prepare(
        &mut self,
        byte_output: Option<UserSlice>,
        slot_output: Option<UserSlice>,
        slot_count: usize,
    ) -> Result<(), CapabilityChannelServiceError> {
        let record_bytes = slot_count
            .checked_mul(CAPABILITY_RECORD_BYTES)
            .ok_or(CapabilityChannelServiceError::InvalidInput)?;
        self._slot_charge = if record_bytes == 0 {
            None
        } else {
            Some(
                self.process
                    .resource_domain()
                    .reserve(
                        ResourceAmount::ZERO
                            .with(ResourceKind::KernelMemoryBytes, record_bytes as u64),
                    )
                    .map_err(ProcessError::from)?
                    .commit(),
            )
        };
        self.slot_bytes = try_zeroed_bytes(record_bytes)?;
        self.process.reserve_user_writes_into(
            [byte_output, slot_output],
            [&mut self.byte_output, &mut self.slot_output],
        )?;
        self.handles = if slot_count == 0 {
            None
        } else {
            Some(self.process.reserve_handle_batch(slot_count)?)
        };
        Ok(())
    }

    fn narrow_handles(&mut self, count: usize) {
        self.handles = match self.handles.take() {
            Some(handles) => self.process.trim_handle_batch(handles, count),
            None if count == 0 => None,
            None => capability_service_invariant("receive contract has no reserved handle slots"),
        };
    }

    fn take_handles(&mut self) -> Option<ProcessHandleBatchReservation> {
        self.handles.take()
    }

    fn write_outputs(
        &mut self,
        bytes: &[u8],
        values: &[HandleValue],
        contracts: &[CapabilitySlotContract],
    ) -> Result<(), ProcessError> {
        if values.len() != contracts.len() {
            capability_service_invariant("destination values and contracts differ");
        }
        if let Some(output) = self.byte_output.as_ref() {
            output.copy_prefix_from(bytes)?;
        } else if !bytes.is_empty() {
            capability_service_invariant("nonempty delivery has no byte output");
        }

        if values.is_empty() {
            return Ok(());
        }
        let records = &mut self.slot_bytes;
        for (index, (value, contract)) in values.iter().zip(contracts).enumerate() {
            let start = match index.checked_mul(CAPABILITY_RECORD_BYTES) {
                Some(start) => start,
                None => capability_service_invariant("slot record offset overflow"),
            };
            let record = match records.get_mut(start..start + CAPABILITY_RECORD_BYTES) {
                Some(record) => record,
                None => capability_service_invariant("slot record exceeded prepared output buffer"),
            };
            encode_receive_slot(record, *value, *contract);
        }
        let used = values.len() * CAPABILITY_RECORD_BYTES;
        match self.slot_output.as_ref() {
            Some(output) => output.copy_prefix_from(&records[..used])?,
            None => capability_service_invariant("nonempty delivery has no slot output"),
        }
        Ok(())
    }

    pub(super) fn abort(mut self) {
        if let Some(handles) = self.handles.take() {
            self.process.abort_handle_batch(handles);
        }
        self.finish_output_access();
    }

    /// Releases the mapping pins after the caller has decided the transaction
    /// outcome. User bytes become visible when copied; `complete` only ends
    /// the address-space mutation exclusion interval.
    fn finish_output_access(&mut self) {
        if let Some(output) = self.byte_output.take() {
            output.complete();
        }
        if let Some(output) = self.slot_output.take() {
            output.complete();
        }
    }
}

impl Drop for CapabilityReceiveTarget {
    fn drop(&mut self) {
        if self.handles.is_some() || self.byte_output.is_some() || self.slot_output.is_some() {
            capability_service_invariant("armed receive target dropped without completion");
        }
    }
}

#[inline(never)]
fn copy_receive_contract(
    process: &Process,
    byte_capacity: usize,
    records: Option<UserSlice>,
) -> Result<(CapabilityReceiveContract, Option<CommittedCharge>), CapabilityChannelServiceError> {
    let record_bytes = slice_length(records)?;
    if record_bytes > MAX_CAPABILITY_RECORD_BUFFER || record_bytes % CAPABILITY_RECORD_BYTES != 0 {
        return Err(CapabilityChannelServiceError::InvalidInput);
    }
    let count = record_bytes / CAPABILITY_RECORD_BYTES;
    // Decoding owns wire storage and a temporary typed vector before the
    // registered contract takes over its own charge. Account this peak too.
    let temporary_bytes = record_bytes + 2 * count * core::mem::size_of::<CapabilitySlotContract>();
    let decode_charge = if temporary_bytes == 0 {
        None
    } else {
        Some(
            process
                .resource_domain()
                .reserve(
                    ResourceAmount::ZERO
                        .with(ResourceKind::KernelMemoryBytes, temporary_bytes as u64),
                )
                .map_err(ProcessError::from)?
                .commit(),
        )
    };
    let mut raw = try_zeroed_bytes(record_bytes)?;
    if let (Some(source), false) = (records, record_bytes == 0) {
        let destination = raw
            .get_mut(..record_bytes)
            .ok_or(CapabilityChannelServiceError::InvalidInput)?;
        process.copy_from_user(source, destination)?;
    }
    let mut contracts = Vec::new();
    contracts
        .try_reserve_exact(count)
        .map_err(|_| CapabilityChannelError::Allocation)?;
    for record in raw.chunks_exact(CAPABILITY_RECORD_BYTES) {
        contracts.push(decode_receive_slot(record)?);
    }
    let contract = CapabilityReceiveContract::new(byte_capacity, &contracts)?;
    Ok((contract, decode_charge))
}

fn try_zeroed_bytes(length: usize) -> Result<Vec<u8>, CapabilityChannelServiceError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| CapabilityChannelError::Allocation)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn decode_send_disposition(
    record: &[u8],
) -> Result<CapabilitySendDisposition, CapabilityChannelServiceError> {
    let decoded = capability_wire::decode_send(record)
        .map_err(|_| CapabilityChannelServiceError::InvalidInput)?;
    let value = handle_wire::from_abi(decoded.handle)
        .map_err(|_| CapabilityChannelServiceError::InvalidInput)?;
    let offered_rights = match decoded.rights {
        RightsOffer::Same => None,
        RightsOffer::Exact(rights) => {
            Some(Rights::from_bits(rights).ok_or(CapabilityChannelServiceError::InvalidInput)?)
        }
    };
    let offered_kind = ObjectKind::try_from_raw(decoded.expected_kind)
        .ok_or(CapabilityChannelServiceError::InvalidInput)?;
    let operation = match decoded.operation {
        Operation::Move => HandleTransferOperation::Move,
        Operation::Duplicate => HandleTransferOperation::Copy,
    };
    Ok(CapabilitySendDisposition {
        value,
        offered_rights,
        offered_kind,
        operation,
    })
}

fn decode_receive_slot(
    record: &[u8],
) -> Result<CapabilitySlotContract, CapabilityChannelServiceError> {
    let decoded = capability_wire::decode_receive(record)
        .map_err(|_| CapabilityChannelServiceError::InvalidInput)?;
    let rights =
        Rights::from_bits(decoded.rights).ok_or(CapabilityChannelServiceError::InvalidInput)?;
    let expected_kind = ObjectKind::try_from_raw(decoded.expected_kind)
        .ok_or(CapabilityChannelServiceError::InvalidInput)?;
    Ok(CapabilitySlotContract {
        rights,
        expected_kind: Some(expected_kind),
    })
}

fn encode_receive_slot(record: &mut [u8], value: HandleValue, contract: CapabilitySlotContract) {
    if record.len() != CAPABILITY_RECORD_BYTES {
        capability_service_invariant("invalid slot output record size");
    }
    let kind = match contract.expected_kind {
        Some(kind) => kind.get(),
        None => capability_service_invariant("process receive contract is untyped"),
    };
    if capability_wire::encode_receive(
        record,
        handle_wire::to_abi(value),
        contract.rights.bits(),
        kind,
    )
    .is_err()
    {
        capability_service_invariant("validated slot contract failed wire encoding");
    }
}

fn slice_length(slice: Option<UserSlice>) -> Result<usize, CapabilityChannelServiceError> {
    slice
        .map(UserSlice::length)
        .unwrap_or(0)
        .try_into()
        .map_err(|_| CapabilityChannelServiceError::InvalidInput)
}

fn channel_error_from_process(error: ProcessError) -> CapabilityChannelError {
    match error {
        ProcessError::Allocation => CapabilityChannelError::Allocation,
        ProcessError::Handle(error) => channel_error_from_handle(error),
        ProcessError::Lifecycle(_) | ProcessError::AddressSpaceReferenced => {
            CapabilityChannelError::BadState
        }
        ProcessError::Resource(error) => CapabilityChannelError::Resource(error),
        ProcessError::UserMemory(_) => CapabilityChannelError::UserMemoryFault,
        ProcessError::Object(_)
        | ProcessError::Scheduler(_)
        | ProcessError::TaskGroup(_)
        | ProcessError::UserEntry(_) => CapabilityChannelError::Internal,
    }
}

const fn channel_error_from_handle(error: HandleError) -> CapabilityChannelError {
    match error {
        HandleError::Allocation => CapabilityChannelError::Allocation,
        HandleError::InvalidHandle => CapabilityChannelError::InvalidHandle,
        HandleError::Busy | HandleError::OutstandingReservation => CapabilityChannelError::Busy,
        HandleError::WrongObjectType => CapabilityChannelError::WrongObjectType,
        HandleError::AccessDenied => CapabilityChannelError::AccessDenied,
        HandleError::UnsupportedTransfer
        | HandleError::UnsupportedRights
        | HandleError::UnsupportedFlags => CapabilityChannelError::UnsupportedTransfer,
        HandleError::ObjectRetired | HandleError::TableRetired | HandleError::EmptyReservation => {
            CapabilityChannelError::BadState
        }
        HandleError::ActiveHandleLimit
        | HandleError::ReservationIdExhausted
        | HandleError::ReservationTooLarge
        | HandleError::TableFull => CapabilityChannelError::ResourceLimit,
        HandleError::ObjectAlreadyActive => CapabilityChannelError::Internal,
    }
}

#[cold]
fn byte_channel_service_invariant(message: &str) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR ByteChannel service invariant failed: {message}"
    ))
}

#[cold]
fn capability_service_invariant(message: &str) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR CapabilityChannel service invariant failed: {message}"
    ))
}
