// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded logical queues and stable, independently waitable batch completions.
//!
//! The device lock precedes a queue lock, which precedes a request lock. None
//! protects user copies or buffer destruction. The pump alone owns request
//! payloads between admission and completion; reapers take completed payloads
//! under a short lock and restore them if the user copy cannot commit.

pub(super) mod payload;
mod scatter;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::authority::Rights;
use crate::fs::primitives::block::Error;
use crate::task::WaitOutcome;
use hyper::mm::{FallibleArc, WeakFallibleArc};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::PreparedTimedWait, crate::object::PublishedTimedWait, crate::object::SignalMask,
    crate::object::SignalSource, crate::object::SignalState, crate::object::SignalWaitOutcome,
    crate::object::TimedWaitPreparation, crate::object::TransferClass,
    crate::object::prepare_timed_wait, crate::object::private, crate::object::wait_one,
};

use super::plan::Operation;
pub(crate) use super::plan::Range;
use super::{Device, Lock, NativeBlock, plan, wire};
use crate::block::queue::payload::Payload;

pub(crate) const COMPLETED: u64 = 1;
pub(crate) const WRITABLE: u64 = 2;
pub(crate) const CLOSED: u64 = 4;
const SIGNALS: u64 = COMPLETED | WRITABLE | CLOSED;

#[derive(Clone, Copy)]
pub(crate) struct QueueLimits {
    pub(crate) capacity: usize,
    pub(crate) byte_limit: usize,
    pub(crate) inflight_limit: usize,
}

impl QueueLimits {
    pub(super) fn valid(self) -> bool {
        (1..=32).contains(&self.capacity)
            && (512..=plan::MAX_DEVICE_BYTES).contains(&self.byte_limit)
            && (1..=wire::REQUEST_QUEUES).contains(&self.inflight_limit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueError {
    Device(Error),
    Invalid,
    WouldBlock,
    NoMemory,
    ResourceLimit,
    TimedOut,
    Cancelled,
    NotFound,
    Busy,
}

impl From<Error> for QueueError {
    fn from(value: Error) -> Self {
        Self::Device(value)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Completion {
    pub(crate) ticket: u64,
    pub(crate) result: Result<(), Error>,
    pub(crate) bytes: usize,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct QueueInfo {
    pub(crate) capacity: u64,
    pub(crate) byte_limit: u64,
    pub(crate) inflight_limit: u64,
    pub(crate) outstanding_requests: u64,
    pub(crate) outstanding_bytes: u64,
    pub(crate) pending_requests: u64,
    pub(crate) inflight_commands: u64,
    pub(crate) completed_requests: u64,
    pub(crate) submitted_batches: u64,
    pub(crate) submitted_ranges: u64,
    pub(crate) dispatched_commands: u64,
    pub(crate) merged_ranges: u64,
    pub(crate) completed_batches: u64,
    pub(crate) peak_inflight: u64,
    pub(crate) device_inflight_limit: u64,
}

pub(super) struct Request {
    #[cfg(feature = "kernel-trace")]
    trace_device: u64,
    pub(super) queue_id: u64,
    pub(super) ticket: AtomicU64,
    pub(super) sequence: AtomicU64,
    // Only the legacy synchronous wrapper abandons requests. Userspace wait
    // deadlines leave their tickets intact for an explicit later reap.
    pub(super) abandoned: AtomicBool,
    pub(super) operation: Operation,
    pub(super) ranges: Vec<Range>,
    pub(super) length: usize,
    pub(super) state: Lock<RequestState>,
    pub(super) signal: SignalState,
    _charge: CommittedCharge,
}

pub(super) struct RequestState {
    pub(super) data: Option<Payload>,
    pub(super) result: Option<Result<(), Error>>,
    pub(super) reaping: bool,
}

impl Request {
    pub(super) fn completion(&self) -> Option<Completion> {
        self.state.with(|state| {
            state.result.map(|result| Completion {
                ticket: self.ticket.load(Ordering::Relaxed),
                result,
                bytes: if result.is_ok() { self.length } else { 0 },
            })
        })
    }
}

pub(super) struct Slot {
    generation: u64,
    reserved: bool,
    pub(super) request: Option<FallibleArc<Request>>,
}

pub(super) struct QueueState {
    pub(super) slots: Vec<Slot>,
    pub(super) bytes: usize,
    pub(super) count: usize,
    pub(super) pending: usize,
    pub(super) inflight: usize,
    pub(super) completed: usize,
    pub(super) revision: u64,
    pub(super) submitted_batches: u64,
    pub(super) submitted_ranges: u64,
    pub(super) dispatched_commands: u64,
    pub(super) merged_ranges: u64,
    pub(super) completed_batches: u64,
    pub(super) peak_inflight: usize,
}

pub(super) struct Queue {
    pub(super) id: u64,
    internal: bool,
    device: WeakFallibleArc<Device>,
    pub(super) limits: QueueLimits,
    pub(super) domain: ResourceDomain,
    pub(super) closed: AtomicBool,
    pub(super) state: Lock<QueueState>,
    pub(super) signals: SignalState,
    pub(super) window: Lock<Option<FallibleArc<super::window::IoWindow>>>,
    _charge: CommittedCharge,
}

impl Queue {
    /// Publish outside accounting locks. A racing publisher retries if a newer
    /// state existed, so its stale snapshot cannot become the final level.
    pub(super) fn refresh_signals(&self) {
        loop {
            let required = self.limits.byte_limit.min(plan::MAX_PAYLOAD);
            let (device_revision, device_failed, device_writable) =
                self.device.upgrade().map_or((0, true, false), |device| {
                    device.state.with(|state| {
                        (
                            state.admission_revision,
                            state.failed,
                            !state.failed && self.device_has_space(state, required),
                        )
                    })
                });
            let (revision, mut bits) = self.state.with(|state| {
                let writable = device_writable
                    && state.count < self.limits.capacity
                    && state.bytes <= self.limits.byte_limit - required;
                (
                    state.revision,
                    (u64::from(state.completed != 0) * COMPLETED)
                        | (u64::from(writable) * WRITABLE),
                )
            });
            if device_failed || self.closed.load(Ordering::Acquire) {
                bits = (bits & COMPLETED) | CLOSED;
            }
            publish(&self.signals, SIGNALS, bits);
            if self.state.with(|state| state.revision == revision)
                && self.device.upgrade().is_none_or(|device| {
                    device
                        .state
                        .with(|state| state.admission_revision == device_revision)
                })
            {
                break;
            }
        }
    }

    pub(super) fn finish(&self, request: &Request, mut data: Payload, result: Result<(), Error>) {
        data.finish();
        self.state.with(|state| {
            request.state.with(|completion| {
                completion.data = Some(data);
                completion.result = Some(result);
                crate::tracepoint!(
                    Io,
                    "block_ready",
                    domain = request.trace_device,
                    request = request.sequence.load(Ordering::Relaxed),
                    status = crate::block::service::queue::completion_status(result) as u64,
                    bytes = if result.is_ok() {
                        request.length as u64
                    } else {
                        0
                    }
                );
            });
            state.completed += 1;
            state.completed_batches = state.completed_batches.saturating_add(1);
            state.revision = state.revision.wrapping_add(1);
        });
        publish(&request.signal, COMPLETED, COMPLETED);
        self.refresh_signals();
    }

    fn device_has_space(&self, device: &super::State, bytes: usize) -> bool {
        plan::Credits {
            requests: device.outstanding,
            bytes: device.bytes,
            public_requests: device.public_outstanding,
            public_bytes: device.public_bytes,
        }
        .admits(self.internal, bytes)
    }
}

pub(crate) struct BlockQueue {
    pub(super) device: FallibleArc<Device>,
    pub(super) queue: FallibleArc<Queue>,
    pub(super) close_on_drop: bool,
}

impl private::Sealed for BlockQueue {}
impl private::UserExportable for BlockQueue {}
impl KernelObject for BlockQueue {
    const KIND: ObjectKind = ObjectKind::BLOCK_QUEUE;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::READ
        .union(Rights::WRITE)
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::TRANSFER)
        .union(Rights::DUPLICATE);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.queue.signals,
            SignalMask::from_trusted_bits(SIGNALS),
        ))
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.close();
    }
}

impl Drop for BlockQueue {
    fn drop(&mut self) {
        if self.close_on_drop {
            self.close();
        }
    }
}

impl NativeBlock {
    pub(crate) fn create_queue(
        &self,
        domain: ResourceDomain,
        limits: QueueLimits,
    ) -> Result<BlockQueue, QueueError> {
        Device::create_queue(&self.device, domain, limits)
    }
}

impl Device {
    pub(super) fn create_queue(
        device: &FallibleArc<Self>,
        domain: ResourceDomain,
        limits: QueueLimits,
    ) -> Result<BlockQueue, QueueError> {
        Self::create_queue_kind(device, domain, limits, false)
    }

    pub(super) fn create_sync_queue(
        device: &FallibleArc<Self>,
        domain: ResourceDomain,
        limits: QueueLimits,
    ) -> Result<BlockQueue, QueueError> {
        Self::create_queue_kind(device, domain, limits, true)
    }

    fn create_queue_kind(
        device: &FallibleArc<Self>,
        domain: ResourceDomain,
        limits: QueueLimits,
        internal: bool,
    ) -> Result<BlockQueue, QueueError> {
        if !limits.valid() {
            return Err(QueueError::Invalid);
        }
        let bytes = crate::object::object_allocation_size::<BlockQueue>()
            .and_then(|n| n.checked_add(FallibleArc::<Queue>::allocation_size()))
            .and_then(|n| n.checked_add(limits.capacity * core::mem::size_of::<Slot>()))
            .ok_or(QueueError::NoMemory)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelMemoryBytes, bytes as u64)
                    .with(ResourceKind::KernelObjects, 1),
            )
            .map_err(|_| QueueError::ResourceLimit)?
            .commit();
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(limits.capacity)
            .map_err(|_| QueueError::NoMemory)?;
        slots.resize_with(limits.capacity, || Slot {
            generation: 0,
            reserved: false,
            request: None,
        });
        let id = device.state.with(|state| {
            if state.failed || state.sectors == 0 {
                return Err(QueueError::Device(Error::Disconnected));
            }
            state.next_queue = state
                .next_queue
                .checked_add(1)
                .ok_or(QueueError::ResourceLimit)?;
            Ok(state.next_queue)
        })?;
        let queue = FallibleArc::try_new(Queue {
            id,
            internal,
            device: device.downgrade(),
            limits,
            domain,
            closed: AtomicBool::new(false),
            state: Lock::new(QueueState {
                slots,
                bytes: 0,
                count: 0,
                pending: 0,
                inflight: 0,
                completed: 0,
                revision: 0,
                submitted_batches: 0,
                submitted_ranges: 0,
                dispatched_commands: 0,
                merged_ranges: 0,
                completed_batches: 0,
                peak_inflight: 0,
            }),
            signals: SignalState::new(),
            window: Lock::new(None),
            _charge: charge,
        })
        .map_err(|_| QueueError::NoMemory)?;
        device.state.with(|state| {
            if state.failed || !state.worker_started {
                return Err(QueueError::Device(Error::Disconnected));
            }
            let slot = state
                .queues
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(QueueError::WouldBlock)?;
            *slot = Some(queue.clone());
            Ok(())
        })?;
        queue.refresh_signals();
        Ok(BlockQueue {
            device: device.clone(),
            queue,
            close_on_drop: true,
        })
    }
}

impl BlockQueue {
    pub(super) fn abandon(&self, ticket: u64) {
        if let Ok(request) = self.find(ticket) {
            request.abandoned.store(true, Ordering::Release);
            self.device.prompt();
        }
    }

    pub(super) fn close(&self) {
        let changed = self
            .device
            .state
            .with(|_| !self.queue.closed.swap(true, Ordering::AcqRel));
        if changed {
            self.queue
                .state
                .with(|state| state.revision = state.revision.wrapping_add(1));
            publish(&self.queue.signals, WRITABLE | CLOSED, CLOSED);
            self.device.notify_space();
            self.device.prompt();
        }
    }

    /// Reserves bounded queue/device credits before allocating or copying a
    /// payload. An unpublished reservation has no device ordering position.
    pub(crate) fn prepare(
        &self,
        operation: u32,
        ranges: Vec<Range>,
    ) -> Result<PreparedSubmission<'_>, QueueError> {
        self.prepare_payload(operation, ranges, None)
    }

    pub(super) fn prepare_payload(
        &self,
        operation: u32,
        ranges: Vec<Range>,
        payload: Option<Payload>,
    ) -> Result<PreparedSubmission<'_>, QueueError> {
        let operation = Operation::decode(operation).ok_or(QueueError::Invalid)?;
        let length = ranges
            .iter()
            .try_fold(0usize, |sum, range| sum.checked_add(range.bytes))
            .ok_or(QueueError::Invalid)?;
        let (sectors, readonly) = self
            .device
            .state
            .with(|state| (state.sectors, state.readonly));
        if readonly && operation == Operation::Write {
            return Err(Error::ReadOnly.into());
        }
        if length > self.queue.limits.byte_limit
            || !plan::validate(operation, &ranges, length, sectors)
        {
            return Err(QueueError::Invalid);
        }
        let (index, ticket) = self.device.state.with(|device| {
            if !self.has_space(device, length)? {
                return Err(QueueError::WouldBlock);
            }
            self.queue.state.with(|state| {
                let (index, slot) = state
                    .slots
                    .iter_mut()
                    .enumerate()
                    .find(|(_, slot)| !slot.reserved && slot.request.is_none())
                    .ok_or(QueueError::WouldBlock)?;
                let generation = slot
                    .generation
                    .checked_add(1)
                    .filter(|generation| *generation < (1u64 << 56))
                    .ok_or(QueueError::ResourceLimit)?;
                slot.generation = generation;
                slot.reserved = true;
                state.count += 1;
                state.bytes += length;
                state.revision = state.revision.wrapping_add(1);
                device.outstanding += 1;
                device.bytes += length;
                if !self.queue.internal {
                    device.public_outstanding += 1;
                    device.public_bytes += length;
                }
                device.admission_revision = device.admission_revision.wrapping_add(1);
                Ok((index, (generation << 8) | (index as u64 + 1)))
            })
        })?;
        let mut prepared = PreparedSubmission {
            owner: self,
            operation,
            ranges: Some(ranges),
            payload: Payload::default(),
            charge: None,
            index,
            ticket,
            length,
            committed: false,
        };
        self.queue.refresh_signals();
        let metadata = prepared.ranges.as_ref().map_or(0, |ranges| {
            ranges.capacity() * core::mem::size_of::<Range>()
        });
        let bytes = FallibleArc::<Request>::allocation_size()
            .checked_add(metadata)
            .and_then(|bytes| {
                bytes.checked_add(match payload.as_ref() {
                    None => length,
                    Some(payload) => payload.allocation_bytes()?,
                })
            })
            .ok_or(QueueError::NoMemory)?;
        prepared.charge = Some(charge(&self.queue.domain, bytes)?);
        prepared.payload = if let Some(payload) = payload {
            payload
        } else {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| QueueError::NoMemory)?;
            bytes.resize(length, 0);
            Payload::Owned(bytes)
        };
        Ok(prepared)
    }

    /// Wait for one request slot and exactly this payload size. This does not
    /// reserve credits: competing submitters must retry admission after waking.
    // Admission wait registration is a separate slow phase. Its publication
    // scratch must not remain below later request commit/completion waits.
    #[inline(never)]
    pub(crate) fn wait_space(
        &self,
        bytes: usize,
        deadline: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), QueueError> {
        if !bytes.is_multiple_of(plan::SECTOR_BYTES)
            || bytes > plan::MAX_PAYLOAD
            || bytes > self.queue.limits.byte_limit
        {
            return Err(QueueError::Invalid);
        }
        enum Publication {
            Immediate(PreparedTimedWait, Result<(), QueueError>),
            Waiting(PublishedTimedWait),
        }
        loop {
            if self
                .device
                .state
                .with(|device| self.has_space(device, bytes))?
            {
                return Ok(());
            }
            let prepared = match prepare_timed_wait(&self.queue.domain, deadline)
                .map_err(|_| QueueError::ResourceLimit)?
            {
                TimedWaitPreparation::Completed(WaitOutcome::TimedOut) => {
                    return Err(QueueError::TimedOut);
                }
                TimedWaitPreparation::Completed(_) => return Err(QueueError::Cancelled),
                TimedWaitPreparation::Armed(prepared) => prepared,
            };
            if cancelled() {
                prepared.request_cancellation();
            }
            // SAFETY: admission, credit return, and close share this condition
            // lock. Publication rechecks the predicate without a check/park
            // gap. The retained IRQ mask is consumed by finish or dropped
            // before abort; neither preparation nor buffer copies occur here.
            let (publication, mask) = unsafe {
                self.device
                    .state
                    .with_mask_retained(|device| match self.has_space(device, bytes) {
                        Ok(false) => Publication::Waiting(
                            prepared.publish_locked(&self.device.space_waiters),
                        ),
                        result => Publication::Immediate(prepared, result.map(|_| ())),
                    })
            };
            match publication {
                Publication::Immediate(prepared, result) => {
                    drop(mask);
                    prepared.abort().map_err(|_| QueueError::ResourceLimit)?;
                    return result;
                }
                Publication::Waiting(published) => {
                    let (outcome, retirement) = published.finish(mask);
                    retirement.map_err(|_| QueueError::ResourceLimit)?;
                    match outcome {
                        WaitOutcome::Notified => {}
                        WaitOutcome::TimedOut => return Err(QueueError::TimedOut),
                        WaitOutcome::Cancelled => return Err(QueueError::Cancelled),
                    }
                }
            }
        }
    }

    fn has_space(&self, device: &super::State, bytes: usize) -> Result<bool, QueueError> {
        if device.failed || self.queue.closed.load(Ordering::Acquire) {
            return Err(Error::Disconnected.into());
        }
        Ok(self.queue.device_has_space(device, bytes)
            && self.queue.state.with(|queue| {
                queue.count < self.queue.limits.capacity
                    && bytes <= self.queue.limits.byte_limit - queue.bytes
            }))
    }

    pub(crate) fn wait(
        &self,
        ticket: u64,
        deadline: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<Completion, QueueError> {
        if ticket == 0 {
            return self.wait_any(deadline, cancelled);
        }
        let request = self.find(ticket)?;
        loop {
            if let Some(completion) = request.completion() {
                return Ok(completion);
            }
            wait_signal(
                &request.signal,
                COMPLETED,
                &self.queue.domain,
                deadline,
                &cancelled,
            )?;
        }
    }

    fn wait_any(
        &self,
        deadline: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<Completion, QueueError> {
        loop {
            let completed = self.queue.state.with(|state| {
                state
                    .slots
                    .iter()
                    .filter_map(|slot| slot.request.as_ref())
                    .find_map(|request| {
                        request.state.with(|state| {
                            if state.reaping {
                                return None;
                            }
                            state.result.map(|result| Completion {
                                ticket: request.ticket.load(Ordering::Relaxed),
                                result,
                                bytes: if result.is_ok() { request.length } else { 0 },
                            })
                        })
                    })
            });
            if let Some(completion) = completed {
                return Ok(completion);
            }
            if self.queue.closed.load(Ordering::Acquire)
                || self.device.state.with(|state| state.failed)
            {
                return Err(Error::Disconnected.into());
            }
            wait_signal(
                &self.queue.signals,
                COMPLETED | CLOSED,
                &self.queue.domain,
                deadline,
                &cancelled,
            )?;
        }
    }

    pub(super) fn find(&self, ticket: u64) -> Result<FallibleArc<Request>, QueueError> {
        let index = (ticket & 255).checked_sub(1).ok_or(QueueError::NotFound)? as usize;
        self.queue.state.with(|state| {
            let slot = state.slots.get(index).ok_or(QueueError::NotFound)?;
            slot.request
                .as_ref()
                .filter(|request| request.ticket.load(Ordering::Relaxed) == ticket)
                .cloned()
                .ok_or(QueueError::NotFound)
        })
    }

    pub(crate) fn operation(&self, ticket: u64) -> Result<u32, QueueError> {
        Ok(self.find(ticket)?.operation.encode())
    }

    pub(crate) fn reap(&self, ticket: u64) -> Result<ReapGuard<'_>, QueueError> {
        let request = self.find(ticket)?;
        let (data, result) = self.queue.state.with(|queue| {
            request.state.with(|state| {
                if state.reaping {
                    return Err(QueueError::Busy);
                }
                let result = state.result.ok_or(QueueError::WouldBlock)?;
                let data = state.data.take().ok_or(QueueError::Busy)?;
                state.reaping = true;
                queue.completed -= 1;
                queue.revision = queue.revision.wrapping_add(1);
                Ok((data, result))
            })
        })?;
        self.queue.refresh_signals();
        Ok(ReapGuard {
            owner: self,
            request,
            data: Some(data),
            result,
            committed: false,
        })
    }

    pub(crate) fn info(&self) -> QueueInfo {
        self.queue.state.with(|state| QueueInfo {
            capacity: self.queue.limits.capacity as u64,
            byte_limit: self.queue.limits.byte_limit as u64,
            inflight_limit: self.queue.limits.inflight_limit as u64,
            outstanding_requests: state.count as u64,
            outstanding_bytes: state.bytes as u64,
            pending_requests: state.pending as u64,
            inflight_commands: state.inflight as u64,
            completed_requests: state.completed as u64,
            submitted_batches: state.submitted_batches,
            submitted_ranges: state.submitted_ranges,
            dispatched_commands: state.dispatched_commands,
            merged_ranges: state.merged_ranges,
            completed_batches: state.completed_batches,
            peak_inflight: state.peak_inflight as u64,
            device_inflight_limit: wire::REQUEST_QUEUES as u64,
        })
    }
}

/// Credits remain reserved across user faults and preemption. Drop rolls back
/// without publishing a request, waking other producers after credits return.
pub(crate) struct PreparedSubmission<'a> {
    owner: &'a BlockQueue,
    operation: Operation,
    ranges: Option<Vec<Range>>,
    payload: Payload,
    charge: Option<CommittedCharge>,
    index: usize,
    ticket: u64,
    length: usize,
    committed: bool,
}
impl PreparedSubmission<'_> {
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        self.payload.owned_mut()
    }
    pub(crate) fn commit(mut self) -> Result<u64, QueueError> {
        let request = FallibleArc::try_new(Request {
            #[cfg(feature = "kernel-trace")]
            trace_device: self.owner.device.trace_id,
            queue_id: self.owner.queue.id,
            ticket: AtomicU64::new(self.ticket),
            sequence: AtomicU64::new(0),
            abandoned: AtomicBool::new(false),
            operation: self.operation,
            length: self.length,
            ranges: self.ranges.take().ok_or(QueueError::Invalid)?,
            state: Lock::new(RequestState {
                data: Some(core::mem::take(&mut self.payload)),
                result: None,
                reaping: false,
            }),
            signal: SignalState::new(),
            _charge: self.charge.take().ok_or(QueueError::Invalid)?,
        })
        .map_err(|_| QueueError::NoMemory)?;
        self.owner.device.state.with(|device| {
            if device.failed || self.owner.queue.closed.load(Ordering::Acquire) {
                return Err(QueueError::Device(Error::Disconnected));
            }
            let sequence = device
                .sequence
                .checked_add(1)
                .ok_or(QueueError::ResourceLimit)?;
            self.owner.queue.state.with(|state| {
                let slot = state.slots.get_mut(self.index).ok_or(QueueError::Invalid)?;
                if !slot.reserved || slot.generation != self.ticket >> 8 {
                    return Err(QueueError::Invalid);
                }
                request.sequence.store(sequence, Ordering::Relaxed);
                slot.reserved = false;
                slot.request = Some(request.clone());
                state.pending += 1;
                state.submitted_batches = state.submitted_batches.saturating_add(1);
                state.submitted_ranges = state
                    .submitted_ranges
                    .saturating_add(request.ranges.len() as u64);
                state.revision = state.revision.wrapping_add(1);
                device.sequence = sequence;
                device.pending.push_back(request.clone());
                // Record admission before the pump can consume this request.
                // Device sequence is independent of queue ticket-slot reuse.
                crate::tracepoint!(
                    Io,
                    "block_queued",
                    domain = self.owner.device.trace_id,
                    request = sequence,
                    queue = request.queue_id,
                    ticket = self.ticket
                );
                crate::tracepoint!(
                    Io,
                    "block_origin",
                    domain = self.owner.device.trace_id,
                    request = sequence,
                    thread = crate::diagnostics::trace::actor(),
                    operation = u64::from(request.operation.encode())
                );
                Ok(())
            })
        })?;
        self.committed = true;
        self.owner.queue.refresh_signals();
        self.owner.device.prompt();
        Ok(self.ticket)
    }
}
impl Drop for PreparedSubmission<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Free private storage before reopening global admission. Waking a
            // competing producer must not transiently exceed the byte bound.
            drop(core::mem::take(&mut self.payload));
            drop(self.ranges.take());
            drop(self.charge.take());
            self.owner.device.state.with(|device| {
                self.owner.queue.state.with(|state| {
                    let Some(slot) = state.slots.get_mut(self.index) else {
                        return;
                    };
                    if slot.reserved && slot.generation == self.ticket >> 8 {
                        slot.reserved = false;
                        state.count -= 1;
                        state.bytes -= self.length;
                        state.revision = state.revision.wrapping_add(1);
                        device.outstanding -= 1;
                        device.bytes -= self.length;
                        if !self.owner.queue.internal {
                            device.public_outstanding -= 1;
                            device.public_bytes -= self.length;
                        }
                        device.admission_revision = device.admission_revision.wrapping_add(1);
                    }
                })
            });
            self.owner.device.notify_space();
            self.owner.queue.refresh_signals();
            self.owner.device.prompt();
        }
    }
}

pub(crate) struct ReapGuard<'a> {
    owner: &'a BlockQueue,
    request: FallibleArc<Request>,
    data: Option<Payload>,
    result: Result<(), Error>,
    committed: bool,
}

impl ReapGuard<'_> {
    pub(crate) fn completion(&self) -> Completion {
        Completion {
            ticket: self.request.ticket.load(Ordering::Relaxed),
            result: self.result,
            bytes: if self.result.is_ok() {
                self.request.length
            } else {
                0
            },
        }
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        if self.request.operation == Operation::Read && self.result.is_ok() {
            self.data.as_ref().map_or(&[], Payload::owned)
        } else {
            &[]
        }
    }
    pub(crate) fn commit(mut self) -> Completion {
        drop(self.data.take());
        let removed = self
            .owner
            .device
            .release_request(&self.owner.queue, &self.request);
        self.committed = true;
        drop(removed);
        self.owner.queue.refresh_signals();
        self.owner.device.prompt();
        self.completion()
    }
}

impl Drop for ReapGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let data = self.data.take();
            self.owner.queue.state.with(|queue| {
                self.request.state.with(|state| {
                    state.data = data;
                    state.reaping = false;
                    queue.completed += 1;
                    queue.revision = queue.revision.wrapping_add(1);
                })
            });
            self.owner.queue.refresh_signals();
            self.owner.device.prompt();
        }
    }
}

impl Device {
    pub(super) fn release_request(
        &self,
        queue: &Queue,
        request: &Request,
    ) -> Option<FallibleArc<Request>> {
        let ticket = request.ticket.load(Ordering::Relaxed);
        let index = ((ticket & 255) - 1) as usize;
        let removed = self.state.with(|device| {
            queue.state.with(|state| {
                let slot = state.slots.get_mut(index)?;
                if slot
                    .request
                    .as_ref()
                    .is_none_or(|current| current.ticket.load(Ordering::Relaxed) != ticket)
                {
                    return None;
                }
                let removed = slot.request.take();
                state.count -= 1;
                state.bytes -= request.length;
                state.revision = state.revision.wrapping_add(1);
                device.outstanding -= 1;
                device.bytes -= request.length;
                if !queue.internal {
                    device.public_outstanding -= 1;
                    device.public_bytes -= request.length;
                }
                device.admission_revision = device.admission_revision.wrapping_add(1);
                crate::tracepoint!(
                    Io,
                    "block_release",
                    domain = self.trace_id,
                    request = request.sequence.load(Ordering::Relaxed),
                    queue = queue.id
                );
                removed
            })
        });
        self.notify_space();
        removed
    }
}

pub(super) fn publish(signal: &SignalState, mask: u64, bits: u64) {
    if signal
        .update(
            SignalMask::from_trusted_bits(mask),
            SignalMask::from_trusted_bits(bits),
        )
        .is_err()
    {
        crate::diagnostics::crash::fatal(format_args!("block queue signal exhausted"));
    }
}

pub(super) fn charge(domain: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, QueueError> {
    domain
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
        .map(|reservation| reservation.commit())
        .map_err(|_| QueueError::ResourceLimit)
}

fn wait_signal(
    signal: &SignalState,
    mask: u64,
    domain: &ResourceDomain,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<(), QueueError> {
    match wait_one(
        SignalSource::new(signal, SignalMask::from_trusted_bits(mask)),
        domain,
        mask,
        deadline,
        cancelled,
    )
    .map_err(|_| QueueError::ResourceLimit)?
    {
        SignalWaitOutcome::Observed(_) => Ok(()),
        SignalWaitOutcome::TimedOut => Err(QueueError::TimedOut),
        SignalWaitOutcome::Cancelled => Err(QueueError::Cancelled),
    }
}
