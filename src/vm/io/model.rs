// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded control records and coalesced notification state; no hardware owners.

pub(crate) const RECORD_BYTES: usize = 256;
pub(crate) const RX_READY: u32 = 1;
pub(crate) const TX_SPACE: u32 = 2;
pub(crate) const CLOSED: u32 = 4;
pub(crate) const ERROR: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Invalid,
    Busy,
    Closed,
    Exhausted,
}

pub(crate) struct Record {
    pub(crate) data: [u8; RECORD_BYTES],
    pub(crate) length: usize,
    pub(crate) sequence: u64,
}
impl Record {
    const fn empty() -> Self {
        Self {
            data: [0; RECORD_BYTES],
            length: 0,
            sequence: 0,
        }
    }
    fn assign(&mut self, bytes: &[u8], sequence: u64) {
        self.data.fill(0);
        self.data[..bytes.len()].copy_from_slice(bytes);
        self.length = bytes.len();
        self.sequence = sequence;
    }
}

pub(crate) struct MailboxState {
    pub(crate) incoming: Record,
    pub(crate) outgoing: Record,
    pub(crate) staging: [u8; RECORD_BYTES],
    pub(crate) staging_length: usize,
    pub(crate) irq_mask: u32,
    pub(crate) error: bool,
    pub(crate) closed: bool,
    claimed: bool,
    next_sequence: u64,
}
impl MailboxState {
    pub(crate) const fn new() -> Self {
        Self {
            incoming: Record::empty(),
            outgoing: Record::empty(),
            staging: [0; RECORD_BYTES],
            staging_length: 0,
            irq_mask: 0,
            error: false,
            closed: false,
            claimed: false,
            next_sequence: 1,
        }
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        let value = self.next_sequence;
        self.next_sequence = value.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(value)
    }
    pub(crate) fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if bytes.is_empty() || bytes.len() > RECORD_BYTES {
            return Err(Error::Invalid);
        }
        if self.outgoing.length != 0 {
            return Err(Error::Busy);
        }
        let sequence = self.sequence()?;
        self.outgoing.assign(bytes, sequence);
        Ok(())
    }
    pub(crate) fn consume(&mut self, sequence: u64) -> Result<(), Error> {
        if self.outgoing.length == 0 || self.outgoing.sequence != sequence {
            return Err(Error::Invalid);
        }
        self.outgoing.length = 0;
        Ok(())
    }
    pub(crate) fn commit_guest(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.incoming.length != 0 {
            return Err(Error::Busy);
        }
        if self.staging_length == 0 || self.staging_length > RECORD_BYTES {
            return Err(Error::Invalid);
        }
        let sequence = self.sequence()?;
        self.incoming
            .assign(&self.staging[..self.staging_length], sequence);
        Ok(())
    }
    pub(crate) fn claim(&mut self, output: &mut [u8; RECORD_BYTES]) -> Result<(usize, u64), Error> {
        if self.claimed {
            return Err(Error::Busy);
        }
        if self.incoming.length == 0 {
            return Err(if self.closed {
                Error::Closed
            } else {
                Error::Busy
            });
        }
        self.claimed = true;
        output.copy_from_slice(&self.incoming.data);
        Ok((self.incoming.length, self.incoming.sequence))
    }
    pub(crate) fn finish_claim(&mut self, sequence: u64, commit: bool) {
        if self.claimed && self.incoming.sequence == sequence {
            if commit {
                self.incoming.length = 0;
            }
            self.claimed = false;
        }
    }
    pub(crate) fn guest_status(&self) -> u32 {
        (if self.outgoing.length != 0 {
            RX_READY
        } else {
            0
        }) | (if self.incoming.length == 0 && !self.closed {
            TX_SPACE
        } else {
            0
        }) | (if self.closed { CLOSED } else { 0 })
            | (if self.error { ERROR } else { 0 })
    }
    pub(crate) fn native_status(&self) -> u64 {
        (if self.incoming.length != 0 && !self.claimed {
            1
        } else {
            0
        }) | (if self.outgoing.length == 0 && !self.closed {
            2
        } else {
            0
        }) | (if self.closed { 4 } else { 0 })
    }
    pub(crate) fn irq(&self) -> bool {
        self.guest_status() & self.irq_mask != 0
    }
}

pub(crate) struct NotificationState {
    pub(crate) interrupt_masked: bool,
    pub(crate) epoch: u32,
    pub(crate) enabled: bool,
    pub(crate) closed: bool,
    pub(crate) kicks: u32,
    pub(crate) status: u32,
}
pub(crate) const QUEUE_COUNT: u64 = hyper::abi::native::HYPER_NATIVE_NATIVE_BLOCK_QUEUE_COUNT;

impl NotificationState {
    pub(crate) const fn new() -> Self {
        Self {
            interrupt_masked: false,
            epoch: 1,
            enabled: false,
            closed: false,
            kicks: 0,
            status: 0,
        }
    }
    pub(crate) fn control(&mut self, operation: u32) -> Result<u32, Error> {
        // Closing a peer already establishes Disable's postcondition; policy
        // must still drain the surviving backend before removing its route.
        if self.closed && operation == 0 {
            return Ok(self.epoch);
        }
        if self.closed && operation != 2 {
            return Err(Error::Closed);
        }
        match operation {
            0 if self.enabled => {
                let next = self.epoch.checked_add(1).ok_or(Error::Exhausted)?;
                self.enabled = false;
                self.epoch = next;
                self.kicks = 0;
                self.status = 0;
            }
            0 => {}
            1 => self.enabled = true,
            2 => self.status |= 2,
            _ => return Err(Error::Invalid),
        }
        Ok(self.epoch)
    }
    pub(crate) fn kick(&mut self, queue: u64) {
        if self.enabled && !self.closed && queue < QUEUE_COUNT {
            self.kicks |= 1 << queue;
        }
    }
    pub(crate) fn take_kicks(&mut self) -> u32 {
        let value = self.kicks;
        self.kicks = 0;
        value
    }
    pub(crate) fn call(&mut self, value: u64) {
        // Completions from a prepared backend may precede activation. Retain
        // them in the reserved epoch, with delivery gated until enable.
        if !self.closed && value >> 32 == self.epoch as u64 {
            self.status |= value as u32 & 3;
        }
    }
    pub(crate) fn ack(&mut self, mask: u32) {
        self.status &= !(mask & 3);
    }
    pub(crate) fn front_irq(&self) -> bool {
        !self.interrupt_masked
            && ((self.status & 2 != 0) || (!self.closed && self.enabled && self.status & 1 != 0))
    }
    pub(crate) fn back_irq(&self) -> bool {
        self.enabled && !self.closed && self.kicks != 0
    }
    pub(crate) fn signals(&self, native: bool) -> u64 {
        u64::from(self.closed)
            | if native {
                u64::from(self.status) << 1
            } else {
                0
            }
    }
    pub(crate) fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.enabled = false;
        self.kicks = 0;
        self.status = 0;
    }
}
