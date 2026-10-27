// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One admitted session owns all queue slots until its whole batch retires.
//! Each queue has one outstanding head, so independent completions may arrive
//! in any order without reusing device-visible buffers or descriptor chains.

use super::{Device, Error, SECTOR_SIZE, Session, SignalWaitOutcome, wait_one, wire};
use core::sync::atomic::{Ordering, fence};

struct Flight<'a> {
    device: &'a Device,
    queue: usize,
    index: u16,
    length: usize,
    read: bool,
    deadline: u64,
    retired: bool,
    #[cfg(feature = "kernel-trace")]
    tag: u64,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        // Closing notifications does not prove DMA quiescence. The dedicated
        // grant stays leased to the backend and no queue can be reused.
        if !self.retired {
            crate::tracepoint!(
                Io,
                "block_abort",
                tag = self.tag,
                domain = self.device.trace_id
            );
            self.device.fail();
        }
    }
}

impl Session<'_> {
    pub(super) fn command(
        &self,
        cdb: &[u8; 16],
        input: Option<&[u8]>,
        output: Option<&mut [u8]>,
    ) -> Result<(), Error> {
        let length = input.map_or_else(|| output.as_ref().map_or(0, |b| b.len()), |b| b.len());
        if input.is_some() && output.is_some() {
            return Err(Error::InvalidRange);
        }
        let flight = self.submit(0, cdb, input, length, output.is_some())?;
        self.kick(1)?;
        flight.finish(output)
    }

    pub(super) fn read_batch(&self, first: u64, output: &mut [u8]) -> Result<(), Error> {
        if output.len() > wire::DATA_BYTES * wire::REQUEST_QUEUES {
            return Err(Error::InvalidRange);
        }
        let mut flights: [Option<Flight<'_>>; wire::REQUEST_QUEUES] =
            core::array::from_fn(|_| None);
        let mut mask = 0;
        for (queue, bytes) in output.chunks(wire::DATA_BYTES).enumerate() {
            let sector = first + (queue * wire::DATA_BYTES / SECTOR_SIZE) as u64;
            let cdb = wire::transfer_cdb(false, sector, (bytes.len() / SECTOR_SIZE) as u32);
            flights[queue] = Some(self.submit(queue, &cdb, None, bytes.len(), true)?);
            mask |= 1 << queue;
        }
        if mask == 0 {
            return Ok(());
        }
        // Publish every request before a single coalesced notification. The
        // sole session waiter rechecks each durable used index after wakeup.
        self.kick(mask)?;
        for (queue, bytes) in output.chunks_mut(wire::DATA_BYTES).enumerate() {
            flights[queue]
                .take()
                .ok_or(Error::Corrupt)?
                .finish(Some(bytes))?;
        }
        Ok(())
    }

    pub(super) fn write_batch(
        &self,
        requests: &[crate::fs::primitives::block::WriteRequest<'_>],
    ) -> Result<(), Error> {
        let mut chunks = requests.iter().flat_map(|request| {
            request
                .bytes
                .chunks(wire::DATA_BYTES)
                .enumerate()
                .map(move |(index, bytes)| {
                    (
                        request.first + (index * wire::DATA_BYTES / SECTOR_SIZE) as u64,
                        bytes,
                    )
                })
        });
        loop {
            let mut flights: [Option<Flight<'_>>; wire::REQUEST_QUEUES] =
                core::array::from_fn(|_| None);
            let mut mask = 0;
            for (queue, flight) in flights.iter_mut().enumerate() {
                let Some((sector, bytes)) = chunks.next() else {
                    break;
                };
                let cdb = wire::transfer_cdb(true, sector, (bytes.len() / SECTOR_SIZE) as u32);
                *flight = Some(self.submit(queue, &cdb, Some(bytes), bytes.len(), false)?);
                mask |= 1 << queue;
            }
            if mask == 0 {
                return Ok(());
            }
            self.kick(mask)?;
            // Each queue keeps its own input grant until completion. On an
            // early failure remaining Flights quarantine the whole device,
            // exactly as in read_batch; no in-flight grant can be reused.
            for flight in flights.into_iter().flatten() {
                flight.finish(None)?;
            }
        }
    }

    fn kick(&self, mask: u32) -> Result<(), Error> {
        fence(Ordering::SeqCst);
        self.device
            .notification
            .kick_native_mask(mask << 2)
            .map_err(|_| Error::Disconnected)
    }

    fn submit(
        &self,
        queue: usize,
        cdb: &[u8; 16],
        input: Option<&[u8]>,
        length: usize,
        read: bool,
    ) -> Result<Flight<'_>, Error> {
        if queue >= wire::REQUEST_QUEUES || length > wire::DATA_BYTES {
            return Err(Error::InvalidRange);
        }
        let d = self.device;
        let (index, tag) = d.state.with(|state| {
            if state.failed {
                return Err(Error::Disconnected);
            }
            state.tag = state.tag.checked_add(1).ok_or(Error::Exhausted)?;
            Ok((state.indices[queue], state.tag))
        })?;
        let ring = wire::REQUEST_QUEUE + queue as u64 * wire::QUEUE_STRIDE;
        let request = wire::REQUEST + queue as u64 * 256;
        let response = request + 128;
        let data = wire::DATA + (queue * wire::DATA_BYTES) as u64;
        d.write(request, &wire::request(tag, cdb))?;
        let mut empty = [0; wire::RESPONSE_BYTES];
        empty[11] = 0xff;
        d.write(response, &empty)?;
        if let Some(bytes) = input {
            d.write(data, bytes)?;
        }
        let response_id = if input.is_some() { 2 } else { 1 };
        d.write(ring, &wire::descriptor(d.guest_base + request, 51, 1, 1))?;
        if input.is_some() {
            d.write(
                ring + 16,
                &wire::descriptor(d.guest_base + data, length as u32, 1, 2),
            )?;
        }
        d.write(
            ring + response_id * 16,
            &wire::descriptor(
                d.guest_base + response,
                wire::RESPONSE_BYTES as u32,
                if read { 3 } else { 2 },
                2,
            ),
        )?;
        if read {
            d.write(
                ring + 32,
                &wire::descriptor(d.guest_base + data, length as u32, 2, 0),
            )?;
        }
        let slot = u64::from(index % wire::QUEUE_SIZE);
        d.write(
            wire::AVAILABLE + queue as u64 * wire::QUEUE_STRIDE + 4 + slot * 2,
            &0u16.to_le_bytes(),
        )?;
        let deadline = crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::Io)?
            .checked_add(30_000_000_000)
            .ok_or(Error::Io)?;
        let flight = Flight {
            device: d,
            queue,
            index,
            length,
            read,
            deadline,
            retired: false,
            #[cfg(feature = "kernel-trace")]
            tag,
        };
        // Linux owns physical DMA synchronization. This release publishes CPU
        // descriptor contents before its coherent view of the available index.
        fence(Ordering::Release);
        d.index_word(wire::AVAILABLE + queue as u64 * wire::QUEUE_STRIDE + 2)?
            .store(index.wrapping_add(1).to_le(), Ordering::Release);
        crate::tracepoint!(
            Io,
            "block_submit",
            tag = tag,
            domain = d.trace_id,
            bytes = length as u64,
            read = u64::from(read)
        );
        Ok(flight)
    }
}

impl Flight<'_> {
    fn finish(mut self, output: Option<&mut [u8]>) -> Result<(), Error> {
        let d = self.device;
        if self.read != output.is_some() || output.as_ref().is_some_and(|b| b.len() != self.length)
        {
            return Err(Error::InvalidRange);
        }
        let used = wire::USED + self.queue as u64 * wire::QUEUE_STRIDE;
        let used_index = used + 2;
        loop {
            let complete = d.u16(used_index)? != self.index;
            let now = crate::time::monotonic_nanoseconds().map_err(|_| Error::Io)?;
            match wire::command_pending(complete, now, self.deadline) {
                Ok(false) => break,
                Ok(true) => {}
                Err(()) => return Err(Error::Disconnected),
            }
            d.notification.acknowledge_native();
            fence(Ordering::SeqCst);
            if d.u16(used_index)? != self.index {
                break;
            }
            let result = wait_one(
                d.notification.native_signal_source(),
                &d.domain,
                7,
                self.deadline,
                || false,
            );
            match result {
                Ok(SignalWaitOutcome::Observed(value)) if value.signals().bits() & 5 == 0 => {}
                _ => return Err(Error::Disconnected),
            }
        }
        fence(Ordering::Acquire);
        let slot = u64::from(self.index % wire::QUEUE_SIZE);
        let mut used = [0; 8];
        d.read(
            wire::USED + self.queue as u64 * wire::QUEUE_STRIDE + 4 + slot * 8,
            &mut used,
        )?;
        let id = u32::from_le_bytes(used[..4].try_into().map_err(|_| Error::Corrupt)?);
        let written =
            u32::from_le_bytes(used[4..].try_into().map_err(|_| Error::Corrupt)?) as usize;
        let mut response = [0; wire::RESPONSE_BYTES];
        d.read(wire::RESPONSE + self.queue as u64 * 256, &mut response)?;
        let result = wire::validate_completion(
            self.index,
            d.u16(used_index)?,
            id,
            written,
            &response,
            self.length,
            self.read,
        );
        if result == Err(wire::CompletionError::Corrupt) {
            return Err(Error::Corrupt);
        }
        d.state
            .with(|state| state.indices[self.queue] = self.index.wrapping_add(1));
        self.retired = true;
        crate::tracepoint!(
            Io,
            "block_complete",
            tag = self.tag,
            domain = self.device.trace_id,
            success = u64::from(result.is_ok())
        );
        if result == Err(wire::CompletionError::Scsi) {
            return Err(Error::Io);
        }
        if let Some(bytes) = output {
            d.read(wire::DATA + (self.queue * wire::DATA_BYTES) as u64, bytes)?;
        }
        Ok(())
    }
}
