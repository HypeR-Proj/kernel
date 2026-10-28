// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One sleeping completion owner per device. Request payloads and DMA slots
//! have separate lifetimes: caller abandonment never recycles a live DMA slot.

use super::{
    Device, Error,
    plan::{self, Cursor, Operation, Range},
    queue::{self, Queue, QueueError, Request},
    request::{self, Flight, Transport},
    wire,
};
use crate::accounting::CommittedCharge;
use crate::task::scheduler;
use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{Ordering, fence};
use hyper::mm::{FallibleArc, try_box};
use {
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalWaitManyOutcome,
    crate::object::SignalWaitRequest, crate::object::wait_many,
};

struct Active {
    queue: FallibleArc<Queue>,
    request: FallibleArc<Request>,
    data: crate::block::queue::payload::Payload,
    cursor: Cursor,
    pieces: usize,
    flush_submitted: bool,
    result: Result<(), Error>,
}

impl Active {
    fn unsent(&self) -> bool {
        self.result.is_ok()
            && !self.queue.closed.load(Ordering::Acquire)
            && !self.request.abandoned.load(Ordering::Acquire)
            && if self.request.operation == Operation::Flush {
                !self.flush_submitted
            } else {
                !self.cursor.exhausted(&self.request.ranges)
            }
    }
}

#[derive(Clone, Copy)]
struct Piece {
    active: usize,
    offset: usize,
    length: usize,
}
struct Command {
    flight: Option<Flight>,
    queue: Option<FallibleArc<Queue>>,
    operation: Operation,
    range: Range,
    pieces: Vec<Piece>,
}

struct Pump {
    device: FallibleArc<Device>,
    transport: Transport,
    active: Vec<Option<Active>>,
    commands: Vec<Command>,
    data_slots: plan::DataSlots,
    _charge: CommittedCharge,
}

pub(super) fn start(device: &FallibleArc<Device>, transport: Transport) -> Result<(), QueueError> {
    let bytes = core::mem::size_of::<Pump>()
        + plan::MAX_REQUESTS * core::mem::size_of::<Option<Active>>()
        + wire::REQUEST_QUEUES
            * (core::mem::size_of::<Command>() + plan::MAX_PIECES * core::mem::size_of::<Piece>());
    let charge = queue::charge(&device.domain, bytes)?;
    let mut active = Vec::new();
    active
        .try_reserve_exact(plan::MAX_REQUESTS)
        .map_err(|_| QueueError::NoMemory)?;
    active.resize_with(plan::MAX_REQUESTS, || None);
    let mut commands = Vec::new();
    commands
        .try_reserve_exact(wire::REQUEST_QUEUES)
        .map_err(|_| QueueError::NoMemory)?;
    for _ in 0..wire::REQUEST_QUEUES {
        let mut pieces = Vec::new();
        pieces
            .try_reserve_exact(plan::MAX_PIECES)
            .map_err(|_| QueueError::NoMemory)?;
        commands.push(Command {
            flight: None,
            queue: None,
            operation: Operation::Read,
            range: Range { first: 0, bytes: 0 },
            pieces,
        });
    }
    let pump = try_box(Pump {
        device: device.clone(),
        transport,
        active,
        commands,
        data_slots: plan::DataSlots::new(wire::REQUEST_QUEUES),
        _charge: charge,
    })
    .map_err(|_| QueueError::NoMemory)?;
    let address = Box::into_raw(pump) as usize;
    let thread = match scheduler::kthread_create("block-io", worker, address) {
        Ok(thread) => thread,
        Err(_) => {
            // SAFETY: Thread creation failed before publishing an execution
            // owner. This is the sole pointer produced by Box::into_raw above.
            drop(unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut::<Pump>(address)) });
            return Err(QueueError::NoMemory);
        }
    };
    device.state.with(|state| state.worker_started = true);
    if !matches!(scheduler::thread_ready(thread), Ok(true)) {
        crate::diagnostics::crash::fatal(format_args!("block worker publication failed"));
    }
    Ok(())
}

extern "C" fn worker(address: usize) {
    // SAFETY: start published exactly this initialized Box to one dormant
    // thread. After publication only this entry owns and finally drops it.
    let mut pump =
        unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut::<Pump>(address)) };
    if pump.run().is_err() {
        pump.device.fail();
        pump.fail_all();
        pump.drain_failed();
    }
    pump.device.state.with(|state| state.worker_started = false);
}

impl Pump {
    fn run(&mut self) -> Result<(), Error> {
        loop {
            // Clear prompts before observing durable queues. A concurrent
            // submission either appears in this pass or leaves WORK asserted.
            queue::publish(&self.device.work, 1, 0);
            self.device.notification.acknowledge_native();
            fence(Ordering::SeqCst);
            if self.device.state.with(|state| state.failed) {
                return Err(Error::Disconnected);
            }
            self.collect()?;
            self.finish_requests();
            self.retire_discarded();
            let mut kicks = 0;
            for slot in 0..self.commands.len() {
                if self.commands[slot].flight.is_none() && self.dispatch(slot)? {
                    kicks |= 1 << slot;
                }
            }
            if kicks != 0 {
                request::kick(&self.device, kicks)?;
            }
            self.finish_requests();
            self.retire_discarded();
            self.refresh_queues();
            let done = self
                .device
                .state
                .with(|state| !state.capability_alive && state.queues.iter().all(Option::is_none));
            if done {
                self.device.notification.close_native();
                return Ok(());
            }
            // Completion can race acknowledgement, scanning, or submission.
            // Scan again before sleeping; the route's level notification covers
            // completions after this scan and wait_many arms/rechecks both.
            if self.has_completion()? {
                continue;
            }
            let deadline = self
                .commands
                .iter()
                .filter_map(|command| command.flight.as_ref())
                .map(|flight| flight.deadline)
                .min()
                .unwrap_or(u64::MAX);
            let waits = [
                SignalWaitRequest::new(
                    SignalSource::new(&self.device.work, SignalMask::from_trusted_bits(1)),
                    1,
                ),
                SignalWaitRequest::new(self.device.notification.native_signal_source(), 7),
            ];
            match wait_many(&waits, &self.device.domain, deadline, || false)
                .map_err(|_| Error::Exhausted)?
            {
                SignalWaitManyOutcome::Observed { index: 1, snapshot }
                    if snapshot.signals().bits() & 5 != 0 =>
                {
                    return Err(Error::Disconnected);
                }
                SignalWaitManyOutcome::Observed { .. } => {}
                SignalWaitManyOutcome::TimedOut => {
                    // An observed completion wins over a simultaneous deadline.
                    if !self.has_completion()? {
                        return Err(Error::Disconnected);
                    }
                }
                SignalWaitManyOutcome::Cancelled => return Err(Error::Disconnected),
            }
            scheduler::cond_resched().map_err(|_| Error::Io)?;
        }
    }

    fn has_completion(&self) -> Result<bool, Error> {
        for command in &self.commands {
            if let Some(flight) = &command.flight
                && flight.ready(&self.device)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn collect(&mut self) -> Result<(), Error> {
        let now = crate::time::monotonic_nanoseconds().map_err(|_| Error::Io)?;
        for command in &mut self.commands {
            let Some(flight) = command.flight.as_mut() else {
                continue;
            };
            let Some(result) = self.transport.complete(&self.device, flight)? else {
                if now >= flight.deadline {
                    return Err(Error::Disconnected);
                }
                continue;
            };
            let mut data_offset = 0;
            for piece in &command.pieces {
                let active = self
                    .active
                    .get_mut(piece.active)
                    .and_then(Option::as_mut)
                    .ok_or(Error::Corrupt)?;
                if result.is_ok() && command.operation == Operation::Read {
                    active.data.read_from_device(
                        &self.device,
                        wire::DATA + (flight.data.first * wire::DATA_BYTES + data_offset) as u64,
                        piece.offset,
                        piece.length,
                    )?;
                }
                data_offset += piece.length;
                active.pieces -= 1;
                if let Err(error) = result {
                    active.result = Err(error);
                }
            }
            if let Some(queue) = command.queue.take() {
                queue.state.with(|state| state.inflight -= 1);
            }
            if flight.data.count != 0 && !self.data_slots.release(flight.data) {
                return Err(Error::Corrupt);
            }
            command.flight = None;
            command.pieces.clear();
        }
        Ok(())
    }

    fn finish_requests(&mut self) {
        for slot in &mut self.active {
            let Some(active) = slot.as_mut() else {
                continue;
            };
            if active.queue.closed.load(Ordering::Acquire)
                || active.request.abandoned.load(Ordering::Acquire)
            {
                active.result = Err(Error::Disconnected);
            }
            if active.pieces != 0 || active.unsent() {
                continue;
            }
            if let Some(active) = slot.take() {
                active
                    .queue
                    .finish(&active.request, active.data, active.result);
            }
        }
    }

    /// FIFO selection is the initial arbitration policy. It deliberately does
    /// not skip an older blocked range: doing so would require checking both
    /// pending and in-flight dependencies before any later overlap can pass.
    fn next(&mut self) -> Result<Option<usize>, Error> {
        if let Some(index) = self
            .active
            .iter()
            .enumerate()
            .filter_map(|(index, active)| {
                active
                    .as_ref()
                    .filter(|active| active.unsent())
                    .map(|active| (index, active.request.sequence.load(Ordering::Relaxed)))
            })
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(index, _)| index)
        {
            return Ok(Some(index));
        }
        loop {
            let pending = self.device.state.with(|state| {
                let request = state.pending.pop_front()?;
                let queue = state
                    .queues
                    .iter()
                    .flatten()
                    .find(|queue| queue.id == request.queue_id)?
                    .clone();
                queue.state.with(|state| state.pending -= 1);
                Some((queue, request))
            });
            let Some((queue, request)) = pending else {
                return Ok(None);
            };
            let data = request
                .state
                .with(|state| state.data.take())
                .ok_or(Error::Corrupt)?;
            if queue.closed.load(Ordering::Acquire) || request.abandoned.load(Ordering::Acquire) {
                queue.finish(&request, data, Err(Error::Disconnected));
                continue;
            }
            let slot = self
                .active
                .iter()
                .position(Option::is_none)
                .ok_or(Error::Corrupt)?;
            self.active[slot] = Some(Active {
                queue,
                request,
                data,
                cursor: Cursor::default(),
                pieces: 0,
                flush_submitted: false,
                result: Ok(()),
            });
            return Ok(Some(slot));
        }
    }

    fn dispatch(&mut self, slot: usize) -> Result<bool, Error> {
        let Some(index) = self.next()? else {
            return Ok(false);
        };
        let mut span = self
            .data_slots
            .largest_free()
            .unwrap_or(plan::DataSpan { first: 0, count: 0 });
        let active = self.active[index].as_ref().ok_or(Error::Corrupt)?;
        let queue = active.queue.clone();
        if queue
            .state
            .with(|state| state.inflight >= queue.limits.inflight_limit)
        {
            return Ok(false);
        }
        let operation = active.request.operation;
        if operation == Operation::Flush {
            if self.commands.iter().any(|command| command.flight.is_some()) {
                return Ok(false);
            }
            let active = self.active[index].as_mut().ok_or(Error::Corrupt)?;
            active.flush_submitted = true;
            active.pieces += 1;
            self.commands[slot].pieces.push(Piece {
                active: index,
                offset: 0,
                length: 0,
            });
            self.commands[slot].range = Range { first: 0, bytes: 0 };
            span = plan::DataSpan { first: 0, count: 0 };
        } else {
            if span.count == 0 {
                return Ok(false);
            }
            let maximum = span.count * wire::DATA_BYTES;
            let (first, _) = active
                .cursor
                .next(&active.request.ranges, maximum)
                .ok_or(Error::Corrupt)?;
            let mut length = 0;
            let mut next = Some(index);
            while let Some(index) = next {
                let active = self.active[index].as_ref().ok_or(Error::Corrupt)?;
                if active.queue.id != queue.id || active.request.operation != operation {
                    break;
                }
                let Some((range, offset)) =
                    active.cursor.next(&active.request.ranges, maximum - length)
                else {
                    break;
                };
                if range.first != first.first + (length / plan::SECTOR_BYTES) as u64 {
                    break;
                }
                if !plan::eligible(
                    operation,
                    range,
                    self.commands
                        .iter()
                        .filter(|command| command.flight.is_some())
                        .map(|command| (command.operation, command.range)),
                ) {
                    break;
                }
                if operation == Operation::Write {
                    active.data.write_to_device(
                        &self.device,
                        offset,
                        wire::DATA + (span.first * wire::DATA_BYTES + length) as u64,
                        range.bytes,
                    )?;
                }
                let active = self.active[index].as_mut().ok_or(Error::Corrupt)?;
                active.cursor.advance(&active.request.ranges, range.bytes);
                active.pieces += 1;
                self.commands[slot].pieces.push(Piece {
                    active: index,
                    offset,
                    length: range.bytes,
                });
                length += range.bytes;
                if length == maximum || self.commands[slot].pieces.len() == plan::MAX_PIECES {
                    break;
                }
                next = self.next()?;
            }
            if length == 0 {
                return Ok(false);
            }
            self.commands[slot].range = Range {
                first: first.first,
                bytes: length,
            };
            span.count = length.div_ceil(wire::DATA_BYTES);
            if !self.data_slots.claim(span) {
                return Err(Error::Corrupt);
            }
        }
        let command = &mut self.commands[slot];
        let cdb = if operation == Operation::Flush {
            let mut cdb = [0; 16];
            cdb[0] = 0x91;
            cdb
        } else {
            wire::transfer_cdb(
                operation == Operation::Write,
                command.range.first,
                (command.range.bytes / plan::SECTOR_BYTES) as u32,
            )
        };
        command.operation = operation;
        command.flight = Some(self.transport.submit(
            &self.device,
            slot,
            span,
            &cdb,
            command.range.bytes,
            operation == Operation::Read,
        )?);
        // One command can cover several batches; one batch can require several
        // commands. Retain those exact edges rather than attributing all I/O to
        // the completion worker's Thread. The loop disappears without tracing.
        #[cfg(feature = "kernel-trace")]
        if let Some(ticket) = hyper_trace::enabled(hyper_trace::Category::Io)
            && let Some(flight) = &command.flight
        {
            for piece in &command.pieces {
                if let Some(active) = self.active.get(piece.active).and_then(Option::as_ref) {
                    flight.trace_link(ticket, &active.request, piece.length);
                }
            }
        }
        queue.state.with(|state| {
            state.inflight += 1;
            state.peak_inflight = state.peak_inflight.max(state.inflight);
            state.dispatched_commands = state.dispatched_commands.saturating_add(1);
            state.merged_ranges = state
                .merged_ranges
                .saturating_add(command.pieces.len().saturating_sub(1) as u64);
        });
        command.queue = Some(queue);
        Ok(true)
    }

    fn refresh_queues(&self) {
        for index in 0..plan::MAX_QUEUES {
            let queue = self.device.state.with(|state| state.queues[index].clone());
            if let Some(queue) = queue {
                queue.refresh_signals();
            }
        }
    }

    fn retire_discarded(&self) {
        for index in 0..plan::MAX_QUEUES {
            let queue = self.device.state.with(|state| state.queues[index].clone());
            let Some(queue) = queue else {
                continue;
            };
            for slot in 0..queue.limits.capacity {
                let request = queue.state.with(|state| {
                    let request = state.slots.get(slot)?.request.as_ref()?;
                    if !queue.closed.load(Ordering::Acquire)
                        && !request.abandoned.load(Ordering::Acquire)
                    {
                        return None;
                    }
                    // Claim and completed-count removal form one transaction
                    // against concurrent reap and failed-user-copy rollback.
                    let claimed = request.state.with(|state| {
                        if state.result.is_none() || state.reaping {
                            false
                        } else {
                            state.reaping = true;
                            true
                        }
                    });
                    if !claimed {
                        return None;
                    }
                    let request = request.clone();
                    state.completed -= 1;
                    state.revision = state.revision.wrapping_add(1);
                    Some(request)
                });
                if let Some(request) = request {
                    let data = request.state.with(|state| state.data.take());
                    drop(data);
                    let removed = self.device.release_request(&queue, &request);
                    drop(removed);
                }
            }
            let removed = self.device.state.with(|state| {
                if queue.closed.load(Ordering::Acquire)
                    && queue.state.with(|state| state.count == 0)
                {
                    state.queues[index].take()
                } else {
                    None
                }
            });
            drop(removed);
        }
    }

    fn drain_failed(&self) {
        loop {
            queue::publish(&self.device.work, 1, 0);
            self.retire_discarded();
            self.refresh_queues();
            if self
                .device
                .state
                .with(|state| !state.capability_alive && state.queues.iter().all(Option::is_none))
            {
                return;
            }
            // Failed device state is permanent, but completed tickets remain
            // readable until their owners reap or close them. Keeping the
            // worker asleep also covers a concurrent preparation/reap rollback.
            if crate::object::wait_one(
                SignalSource::new(&self.device.work, SignalMask::from_trusted_bits(1)),
                &self.device.domain,
                1,
                u64::MAX,
                || false,
            )
            .is_err()
            {
                // Resource pressure must not discard pending cleanup ownership.
                let _ = scheduler::yield_now();
            }
        }
    }

    fn fail_all(&mut self) {
        // Device-visible storage is exclusively the persistent bounce grant.
        // Terminating logical requests does not free or recycle that grant;
        // the backend VM mapping retains its DMA lease through retirement.
        for command in &mut self.commands {
            if let Some(queue) = command.queue.take() {
                queue.state.with(|state| state.inflight -= 1);
            }
            command.flight = None;
            command.pieces.clear();
        }
        for active in &mut self.active {
            if let Some(active) = active.take() {
                active
                    .queue
                    .finish(&active.request, active.data, Err(Error::Disconnected));
            }
        }
        loop {
            let next = self.device.state.with(|state| {
                let request = state.pending.pop_front()?;
                let queue = state
                    .queues
                    .iter()
                    .flatten()
                    .find(|queue| queue.id == request.queue_id)?
                    .clone();
                queue.state.with(|state| state.pending -= 1);
                Some((queue, request))
            });
            let Some((queue, request)) = next else {
                break;
            };
            let data = request
                .state
                .with(|state| state.data.take())
                .unwrap_or_default();
            queue.finish(&request, data, Err(Error::Disconnected));
        }
        for index in 0..plan::MAX_QUEUES {
            let queue = self.device.state.with(|state| state.queues[index].clone());
            if let Some(queue) = queue {
                queue.refresh_signals();
            }
        }
        self.retire_discarded();
    }
}
