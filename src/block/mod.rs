// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native block devices, bounded logical queues and asynchronous transport.
//! Userspace negotiates the backend; one device pump owns its physical rings.

mod plan;
mod pump;
mod queue;
mod request;
pub(crate) mod service;
mod transfer;
mod window;
mod wire;

pub(crate) use window::{IoWindow, WindowBinding};

use crate::authority::Rights;
use crate::mm::user_space::GuestMemoryBacking;
use crate::vm::io::Notification;
use alloc::{collections::VecDeque, vec::Vec};
use core::sync::atomic::{AtomicU16, Ordering, fence};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::fs::primitives::block::BlockDevice, crate::fs::primitives::block::Error,
    crate::fs::primitives::block::SECTOR_SIZE,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::SignalSource, crate::object::SignalState, crate::object::SignalWaitOutcome,
    crate::object::TransferClass, crate::object::private, crate::object::wait_one,
};

pub(crate) use queue::{BlockQueue, QueueError, QueueLimits, Range};
type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

struct State {
    activating: bool,
    failed: bool,
    sectors: u64,
    readonly: bool,
    capability_alive: bool,
    worker_started: bool,
    next_queue: u64,
    sequence: u64,
    outstanding: usize,
    bytes: usize,
    public_outstanding: usize,
    public_bytes: usize,
    admission_revision: u64,
    sync_queue: Option<FallibleArc<queue::Queue>>,
    queues: Vec<Option<FallibleArc<queue::Queue>>>,
    pending: VecDeque<FallibleArc<queue::Request>>,
}

pub(crate) struct Device {
    #[cfg(feature = "kernel-trace")]
    trace_id: u64,
    // Backend VM mappings independently retain the DMA lease. Failure closes
    // admission; it never treats a logical timeout as physical DMA retirement.
    memory: GuestMemoryBacking,
    guest_base: u64,
    notification: Notification,
    // The retained resource sponsor is independent of its creator Process.
    // Bootstrap exit does not end the live Device or its completion worker.
    domain: ResourceDomain,
    state: Lock<State>,
    work: SignalState,
    space_waiters: crate::task::WaitQueue,
    _charge: CommittedCharge,
    _queue_charge: CommittedCharge,
}

impl Device {
    pub(crate) fn new(
        memory: GuestMemoryBacking,
        guest_base: u64,
        notification: Notification,
        domain: ResourceDomain,
        charge: CommittedCharge,
    ) -> Result<Self, QueueError> {
        let queue_charge = queue::charge(
            &domain,
            plan::MAX_QUEUES * core::mem::size_of::<Option<FallibleArc<queue::Queue>>>()
                + plan::MAX_REQUESTS * core::mem::size_of::<FallibleArc<queue::Request>>(),
        )?;
        let mut queues = Vec::new();
        queues
            .try_reserve_exact(plan::MAX_QUEUES)
            .map_err(|_| QueueError::NoMemory)?;
        queues.resize_with(plan::MAX_QUEUES, || None);
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(plan::MAX_REQUESTS)
            .map_err(|_| QueueError::NoMemory)?;
        Ok(Self {
            #[cfg(feature = "kernel-trace")]
            trace_id: next_trace_id()?,
            memory,
            guest_base,
            notification,
            domain,
            state: Lock::new(State {
                activating: false,
                failed: false,
                sectors: 0,
                readonly: false,
                capability_alive: true,
                worker_started: false,
                next_queue: 0,
                sequence: 0,
                outstanding: 0,
                bytes: 0,
                public_outstanding: 0,
                public_bytes: 0,
                admission_revision: 0,
                sync_queue: None,
                queues,
                pending,
            }),
            work: SignalState::new(),
            space_waiters: crate::task::WaitQueue::new(),
            _charge: charge,
            _queue_charge: queue_charge,
        })
    }

    fn prompt(&self) {
        queue::publish(&self.work, 1, 1);
    }

    fn notify_space(&self) {
        if self.space_waiters.wake_all().is_err() {
            crate::diagnostics::crash::fatal(format_args!("block admission wake failed"));
        }
    }

    fn fail(&self) {
        self.state.with(|state| {
            state.failed = true;
            state.admission_revision = state.admission_revision.wrapping_add(1);
        });
        self.notification.close_native();
        self.notify_space();
        self.prompt();
    }

    fn read(&self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        self.memory
            .read_exposed(offset, output)
            .map_err(|_| Error::Io)
    }
    fn write(&self, offset: u64, input: &[u8]) -> Result<(), Error> {
        self.memory
            .write_exposed(offset, input)
            .map_err(|_| Error::Io)
    }
    fn index_word(&self, offset: u64) -> Result<&AtomicU16, Error> {
        if !offset.is_multiple_of(2) || offset + 2 > wire::MEMORY_BYTES {
            return Err(Error::InvalidRange);
        }
        let physical = self
            .memory
            .physical_page(offset & !4095)
            .map_err(|_| Error::Io)?;
        let address = crate::mm::memory::linear_address(physical.get())
            .ok_or(Error::Io)?
            .checked_add((offset & 4095) as usize)
            .ok_or(Error::Io)?;
        // SAFETY: The hardware lease pins this initialized aligned word and
        // its permanent linear map. Each ring index has exactly one producer;
        // all other guest-owned bytes use the external-memory copy contract.
        Ok(unsafe { &*core::ptr::with_exposed_provenance::<AtomicU16>(address) })
    }
    fn u16(&self, offset: u64) -> Result<u16, Error> {
        Ok(u16::from_le(
            self.index_word(offset)?.load(Ordering::Acquire),
        ))
    }

    fn activate(device: &FallibleArc<Self>, readonly: bool) -> Result<u64, Error> {
        device.state.with(|state| {
            if state.failed {
                return Err(Error::Disconnected);
            }
            if state.activating || state.sectors != 0 {
                return Err(Error::InvalidRange);
            }
            state.activating = true;
            Ok(())
        })?;
        match Self::discover(device, readonly) {
            Ok(sectors) => Ok(sectors),
            Err(error) => {
                device.fail();
                Err(error)
            }
        }
    }

    /// Capacity discovery is the only synchronous transport owner. No queue
    /// can be created until it hands its indices to the published pump.
    fn discover(device: &FallibleArc<Self>, readonly: bool) -> Result<u64, Error> {
        device
            .notification
            .control(1)
            .map_err(|_| Error::Disconnected)?;
        let mut transport = request::Transport::new();
        let mut capacity = [0; 32];
        let mut last = Err(Error::Io);
        for _ in 0..4 {
            last = device.discover_command(&mut transport, &mut capacity);
            if last.is_ok() || matches!(last, Err(Error::Disconnected | Error::Corrupt)) {
                break;
            }
        }
        last?;
        let last_sector = u64::from_be_bytes(capacity[..8].try_into().map_err(|_| Error::Corrupt)?);
        let size = u32::from_be_bytes(capacity[8..12].try_into().map_err(|_| Error::Corrupt)?);
        if size != SECTOR_SIZE as u32 {
            return Err(Error::Unsupported);
        }
        let sectors = last_sector
            .checked_add(1)
            .filter(|sectors| *sectors != 0)
            .ok_or(Error::Corrupt)?;
        device.state.with(|state| {
            state.sectors = sectors;
            state.readonly = readonly;
        });
        pump::start(device, transport).map_err(queue_error)?;
        let mut queue = Device::create_sync_queue(
            device,
            device.domain.clone(),
            QueueLimits {
                capacity: 16,
                byte_limit: 2 * 1024 * 1024,
                inflight_limit: wire::REQUEST_QUEUES,
            },
        )
        .map_err(queue_error)?;
        device.state.with(|state| {
            if state.failed || !state.capability_alive {
                return Err(Error::Disconnected);
            }
            state.sync_queue = Some(queue.queue.clone());
            Ok(())
        })?;
        queue.close_on_drop = false;
        Ok(sectors)
    }

    fn discover_command(
        &self,
        transport: &mut request::Transport,
        capacity: &mut [u8; 32],
    ) -> Result<(), Error> {
        let mut flight = transport.submit(
            self,
            0,
            plan::DataSpan { first: 0, count: 1 },
            &wire::capacity_cdb(),
            capacity.len(),
            true,
        )?;
        request::kick(self, 1)?;
        loop {
            if let Some(result) = transport.complete(self, &mut flight)? {
                result?;
                return self.read(wire::DATA, capacity);
            }
            let now = crate::time::monotonic_nanoseconds().map_err(|_| Error::Io)?;
            if now >= flight.deadline {
                return Err(Error::Disconnected);
            }
            self.notification.acknowledge_native();
            fence(Ordering::SeqCst);
            if flight.ready(self)? {
                continue;
            }
            match wait_one(
                self.notification.native_signal_source(),
                &self.domain,
                7,
                flight.deadline,
                || false,
            )
            .map_err(|_| Error::Exhausted)?
            {
                SignalWaitOutcome::Observed(value) if value.signals().bits() & 5 == 0 => {}
                _ => return Err(Error::Disconnected),
            }
        }
    }
}

pub(crate) struct NativeBlock {
    device: FallibleArc<Device>,
}
impl NativeBlock {
    pub(crate) fn activate(&self, readonly: bool) -> Result<u64, Error> {
        Device::activate(&self.device, readonly)
    }
}
impl private::Sealed for NativeBlock {}
impl private::UserExportable for NativeBlock {}
impl KernelObject for NativeBlock {
    const KIND: ObjectKind = ObjectKind::NATIVE_BLOCK;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::READ
        .union(Rights::WRITE)
        .union(Rights::MAP)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::WAIT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(self.device.notification.closed_signal_source())
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        let (queue, worker_started) = self.device.state.with(|state| {
            state.capability_alive = false;
            if !state.worker_started {
                state.failed = true;
                state.admission_revision = state.admission_revision.wrapping_add(1);
            }
            let queue = state.sync_queue.take();
            if let Some(queue) = &queue {
                queue.closed.store(true, Ordering::Release);
            }
            (queue, state.worker_started)
        });
        if let Some(queue) = queue {
            queue
                .state
                .with(|state| state.revision = state.revision.wrapping_add(1));
            queue.refresh_signals();
        }
        if !worker_started {
            self.device.notification.close_native();
        }
        self.device.notify_space();
        self.device.prompt();
    }
}

pub(crate) struct BlockAccess {
    device: FallibleArc<Device>,
}
impl BlockAccess {
    fn queue(&self) -> Result<BlockQueue, Error> {
        let queue = self
            .device
            .state
            .with(|state| state.sync_queue.clone())
            .ok_or(Error::Disconnected)?;
        Ok(BlockQueue {
            device: self.device.clone(),
            queue,
            close_on_drop: false,
        })
    }

    fn prepare<'a>(
        &self,
        queue: &'a BlockQueue,
        operation: u32,
        ranges: &[Range],
    ) -> Result<queue::PreparedSubmission<'a>, Error> {
        loop {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(ranges.len())
                .map_err(|_| Error::Exhausted)?;
            owned.extend_from_slice(ranges);
            match queue.prepare(operation, owned) {
                Ok(prepared) => return Ok(prepared),
                Err(QueueError::WouldBlock) => {
                    let bytes = ranges.iter().map(|range| range.bytes).sum();
                    queue
                        .wait_space(bytes, u64::MAX, || false)
                        .map_err(queue_error)?;
                }
                Err(error) => return Err(queue_error(error)),
            }
        }
    }

    fn complete(
        &self,
        queue: &BlockQueue,
        ticket: u64,
        output: Option<&mut [u8]>,
    ) -> Result<(), Error> {
        let completed = (|| {
            queue
                .wait(ticket, u64::MAX, || false)
                .map_err(queue_error)?;
            let result = queue.reap(ticket).map_err(queue_error)?;
            if let Some(output) = output
                && result.completion().result.is_ok()
            {
                output.copy_from_slice(result.bytes());
            }
            result.commit().result
        })();
        if completed.is_err() {
            // This shared queue outlives a caller. A failed waiter must not
            // leave an unreapable ticket consuming its bounded capacity.
            // The pump still owns and drains every already published DMA.
            queue.abandon(ticket);
        }
        completed
    }
}
impl BlockDevice for BlockAccess {
    fn is_read_only(&self) -> bool {
        self.device.state.with(|state| state.readonly)
    }
    fn sector_count(&self) -> u64 {
        self.device.state.with(|state| state.sectors)
    }
    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), Error> {
        check_range(first, output.len(), self.sector_count())?;
        if output.is_empty() {
            return Ok(());
        }
        let queue = self.queue()?;
        for (index, bytes) in output.chunks_mut(plan::MAX_PAYLOAD).enumerate() {
            let sector = first + (index * plan::MAX_PAYLOAD / SECTOR_SIZE) as u64;
            let prepared = self.prepare(
                &queue,
                0,
                &[Range {
                    first: sector,
                    bytes: bytes.len(),
                }],
            )?;
            let ticket = prepared.commit().map_err(queue_error)?;
            self.complete(&queue, ticket, Some(bytes))?;
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), Error> {
        self.write_batch(&[crate::fs::primitives::block::WriteRequest {
            first,
            bytes: input,
        }])
    }
    fn write_batch(
        &mut self,
        requests: &[crate::fs::primitives::block::WriteRequest<'_>],
    ) -> Result<(), Error> {
        crate::fs::primitives::block::validate_writes(self.sector_count(), requests)?;
        let queue = self.queue()?;
        let mut source = 0;
        let mut offset = 0;
        loop {
            let mut ranges = [Range { first: 0, bytes: 0 }; plan::MAX_RANGES];
            let mut pieces = [&[][..]; plan::MAX_RANGES];
            let mut count = 0;
            let mut total = 0;
            while count < plan::MAX_RANGES && total < plan::MAX_PAYLOAD {
                let Some(request) = requests.get(source) else {
                    break;
                };
                if offset == request.bytes.len() {
                    source += 1;
                    offset = 0;
                    continue;
                }
                let length = (request.bytes.len() - offset).min(plan::MAX_PAYLOAD - total);
                ranges[count] = Range {
                    first: request.first + (offset / SECTOR_SIZE) as u64,
                    bytes: length,
                };
                pieces[count] = &request.bytes[offset..offset + length];
                offset += length;
                total += length;
                count += 1;
            }
            if count == 0 {
                return Ok(());
            }
            let mut prepared = self.prepare(&queue, 1, &ranges[..count])?;
            let mut destination = prepared.bytes_mut();
            for piece in &pieces[..count] {
                let (target, remainder) = destination.split_at_mut(piece.len());
                target.copy_from_slice(piece);
                destination = remainder;
            }
            let ticket = prepared.commit().map_err(queue_error)?;
            self.complete(&queue, ticket, None)?;
        }
    }
    fn flush(&mut self) -> Result<(), Error> {
        let queue = self.queue()?;
        let prepared = self.prepare(&queue, 2, &[])?;
        let ticket = prepared.commit().map_err(queue_error)?;
        self.complete(&queue, ticket, None)
    }
}
fn queue_error(error: QueueError) -> Error {
    match error {
        QueueError::Device(error) => error,
        QueueError::Invalid | QueueError::NotFound => Error::InvalidRange,
        QueueError::TimedOut | QueueError::Cancelled => Error::Disconnected,
        _ => Error::Exhausted,
    }
}
fn check_range(first: u64, bytes: usize, sectors: u64) -> Result<(), Error> {
    crate::fs::primitives::block::validate_range(sectors, first, bytes)
}

// Correlation survives queue index reuse and distinguishes independently created
// devices in the same resource domain. This is never an address or authority.
#[cfg(feature = "kernel-trace")]
fn next_trace_id() -> Result<u64, QueueError> {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| QueueError::ResourceLimit)
}
