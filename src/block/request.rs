// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! The pump's exclusive ownership of the fixed virtio-scsi transport slots.
//! Logical queue capacities are independent of these four physical DMA slots.

use super::{Device, Error, wire};
use core::sync::atomic::{Ordering, fence};

pub(super) struct Transport {
    indices: [u16; wire::REQUEST_QUEUES],
    tag: u64,
}

pub(super) struct Flight {
    pub(super) slot: usize,
    index: u16,
    pub(super) length: usize,
    pub(super) data: super::plan::DataSpan,
    read: bool,
    pub(super) deadline: u64,
    #[cfg(feature = "kernel-trace")]
    trace: (u64, u64),
    #[cfg(feature = "kernel-trace")]
    retired: bool,
}

impl Drop for Flight {
    fn drop(&mut self) {
        // The pump/discovery owner quarantines failed DMA storage. This event
        // only closes the diagnostic lifetime of a published physical command.
        #[cfg(feature = "kernel-trace")]
        if !self.retired {
            crate::tracepoint!(Io, "block_abort", tag = self.trace.0, domain = self.trace.1);
        }
    }
}

impl Flight {
    #[cfg(feature = "kernel-trace")]
    pub(super) fn trace_link(
        &self,
        ticket: hyper_trace::Ticket,
        request: &super::queue::Request,
        bytes: usize,
    ) {
        crate::tracepoint!(
            ticket = ticket,
            Io,
            "block_link",
            domain = self.trace.1,
            tag = self.trace.0,
            request = request.sequence.load(Ordering::Relaxed),
            bytes = bytes as u64
        );
    }

    pub(super) fn ready(&self, device: &Device) -> Result<bool, Error> {
        Ok(device.u16(wire::USED + self.slot as u64 * wire::QUEUE_STRIDE + 2)? != self.index)
    }
}

impl Transport {
    pub(super) const fn new() -> Self {
        Self {
            indices: [0; wire::REQUEST_QUEUES],
            tag: 0,
        }
    }

    /// Every fallible preparation precedes the available-index publication.
    /// The caller retains the DMA lease and never reuses a failed live slot.
    pub(super) fn submit(
        &mut self,
        device: &Device,
        slot: usize,
        data_span: super::plan::DataSpan,
        cdb: &[u8; 16],
        length: usize,
        read: bool,
    ) -> Result<Flight, Error> {
        if slot >= wire::REQUEST_QUEUES
            || data_span.first + data_span.count > wire::REQUEST_QUEUES
            || length > wire::DATA_BYTES * data_span.count
        {
            return Err(Error::InvalidRange);
        }
        let index = self.indices[slot];
        self.tag = self.tag.checked_add(1).ok_or(Error::Exhausted)?;
        let ring = wire::REQUEST_QUEUE + slot as u64 * wire::QUEUE_STRIDE;
        let request = wire::REQUEST + slot as u64 * 256;
        let response = request + 128;
        let data = wire::DATA + (data_span.first * wire::DATA_BYTES) as u64;
        device.write(request, &wire::request(self.tag, cdb))?;
        let mut empty = [0; wire::RESPONSE_BYTES];
        empty[11] = 0xff;
        device.write(response, &empty)?;
        let write = length != 0 && !read;
        let response_id = if write { 2 } else { 1 };
        device.write(
            ring,
            &wire::descriptor(device.guest_base + request, 51, 1, 1),
        )?;
        if write {
            device.write(
                ring + 16,
                &wire::descriptor(device.guest_base + data, length as u32, 1, 2),
            )?;
        }
        device.write(
            ring + response_id * 16,
            &wire::descriptor(
                device.guest_base + response,
                wire::RESPONSE_BYTES as u32,
                if read { 3 } else { 2 },
                2,
            ),
        )?;
        if read {
            device.write(
                ring + 32,
                &wire::descriptor(device.guest_base + data, length as u32, 2, 0),
            )?;
        }
        device.write(
            wire::AVAILABLE
                + slot as u64 * wire::QUEUE_STRIDE
                + 4
                + u64::from(index % wire::QUEUE_SIZE) * 2,
            &0u16.to_le_bytes(),
        )?;
        let deadline = crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::Io)?
            .checked_add(30_000_000_000)
            .ok_or(Error::Io)?;
        let available =
            device.index_word(wire::AVAILABLE + slot as u64 * wire::QUEUE_STRIDE + 2)?;
        let flight = Flight {
            slot,
            index,
            length,
            data: data_span,
            read,
            deadline,
            #[cfg(feature = "kernel-trace")]
            trace: (self.tag, device.trace_id),
            #[cfg(feature = "kernel-trace")]
            retired: false,
        };
        // Coherent shared memory with the Linux vhost CPU. Physical DMA cache
        // synchronization belongs to Linux; Release publishes descriptor bytes.
        available.store(index.wrapping_add(1).to_le(), Ordering::Release);
        crate::tracepoint!(
            Io,
            "block_submit",
            tag = self.tag,
            domain = device.trace_id,
            bytes = length as u64,
            read = u64::from(read)
        );
        crate::tracepoint!(
            Io,
            "block_command",
            domain = device.trace_id,
            tag = self.tag,
            slot = slot as u64,
            opcode = u64::from(cdb[0])
        );
        Ok(flight)
    }

    pub(super) fn complete(
        &mut self,
        device: &Device,
        flight: &mut Flight,
    ) -> Result<Option<Result<(), Error>>, Error> {
        let used = wire::USED + flight.slot as u64 * wire::QUEUE_STRIDE;
        let index = device.u16(used + 2)?;
        if index == flight.index {
            return Ok(None);
        }
        let mut entry = [0; 8];
        device.read(
            used + 4 + u64::from(flight.index % wire::QUEUE_SIZE) * 8,
            &mut entry,
        )?;
        let id = u32::from_le_bytes(entry[..4].try_into().map_err(|_| Error::Corrupt)?);
        let written =
            u32::from_le_bytes(entry[4..].try_into().map_err(|_| Error::Corrupt)?) as usize;
        let mut response = [0; wire::RESPONSE_BYTES];
        device.read(wire::RESPONSE + flight.slot as u64 * 256, &mut response)?;
        let result = wire::validate_completion(
            flight.index,
            index,
            id,
            written,
            &response,
            flight.length,
            flight.read,
        );
        match result {
            Err(wire::CompletionError::Corrupt) => Err(Error::Corrupt),
            result => {
                self.indices[flight.slot] = index;
                #[cfg(feature = "kernel-trace")]
                {
                    flight.retired = true;
                }
                crate::tracepoint!(
                    Io,
                    "block_complete",
                    tag = flight.trace.0,
                    domain = device.trace_id,
                    success = u64::from(result.is_ok())
                );
                Ok(Some(result.map_err(|_| Error::Io)))
            }
        }
    }
}

pub(super) fn kick(device: &Device, slots: u32) -> Result<(), Error> {
    // The notification MMIO route must observe all released available indices
    // before publishing the kick. This preserves the existing route protocol.
    fence(Ordering::SeqCst);
    device
        .notification
        .kick_native_mask(slots << 2)
        .map_err(|_| Error::Disconnected)
}
