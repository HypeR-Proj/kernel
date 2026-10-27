// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Arm SBSA Generic UART byte-stream model (DEN0029, Appendix B).
//!
//! The UART is always enabled, with eight-bit characters, separate 32-byte
//! FIFOs, and fixed interrupt thresholds of four bytes. Configuration registers,
//! DMA, modem signals, and the PL011 identity are deliberately absent. The host
//! transport carries clean bytes, so only receive overrun can generate an error.
//!
//! Transmission completes when the owner accepts a byte into its bounded output
//! transport, not after a simulated wire delay. Baud rate determines the receive
//! timeout only. The owner serializes register access and schedules `next_timeout`.

/// Register offsets and bits in the SBSA Generic UART programming interface.
pub mod reg {
    pub const DR: u64 = 0x000;
    pub const RSR_ECR: u64 = 0x004;
    pub const FR: u64 = 0x018;
    pub const IMSC: u64 = 0x038;
    pub const RIS: u64 = 0x03c;
    pub const MIS: u64 = 0x040;
    pub const ICR: u64 = 0x044;

    pub const DR_OE: u32 = 1 << 11;
    pub const RSR_OE: u32 = 1 << 3;
    pub const FR_BUSY: u32 = 1 << 3;
    pub const FR_RXFE: u32 = 1 << 4;
    pub const FR_TXFF: u32 = 1 << 5;
    pub const FR_RXFF: u32 = 1 << 6;
    pub const FR_TXFE: u32 = 1 << 7;
    pub const INT_RX: u32 = 1 << 4;
    pub const INT_TX: u32 = 1 << 5;
    pub const INT_RT: u32 = 1 << 6;
    pub const INT_OE: u32 = 1 << 10;
    pub const INT_ALL: u32 = 0x7f0;
}

const FIFO_CAPACITY: usize = 32;
const FIFO_TRIGGER: usize = FIFO_CAPACITY / 8;
const REGISTER_FRAME_BYTES: u64 = 0x1000;
/// Fixed firmware configuration shared with the guest device-tree builder.
pub const BAUD: u64 = hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_UART_BAUD;
pub const REFERENCE_BASE: u64 =
    hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_UART_BASE;
pub const REFERENCE_SIZE: u64 =
    hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_UART_SIZE;
pub const REFERENCE_INTERRUPT: u32 =
    hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_UART_INTERRUPT as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessError {
    InvalidAccess,
    /// Guest software must wait for TXFF to clear before writing another byte.
    TransmitFull,
}

pub struct SbsaUart {
    receive_fifo: Fifo,
    transmit_fifo: Fifo,
    receive_status: u32,
    data_overrun: bool,
    interrupt_mask: u32,
    interrupt_status: u32,
    receive_deadline: Option<u64>,
    receive_timeout_ticks: u64,
}

impl Default for SbsaUart {
    fn default() -> Self {
        Self::new()
    }
}

impl SbsaUart {
    /// Creates the fixed-baud UART with nanoseconds as its counter unit.
    pub const fn new() -> Self {
        Self {
            receive_fifo: Fifo::new(),
            transmit_fifo: Fifo::new(),
            receive_status: 0,
            data_overrun: false,
            interrupt_mask: 0,
            interrupt_status: 0,
            receive_deadline: None,
            receive_timeout_ticks: 32_000_000_000_u64.div_ceil(BAUD),
        }
    }

    /// Uses the owner's monotonic counter; the 32-bit-period RX timeout rounds up.
    pub fn with_clock(frequency: u64) -> Self {
        Self {
            receive_timeout_ticks: (u128::from(frequency) * 32)
                .div_ceil(u128::from(BAUD))
                .clamp(1, i64::MAX as u128) as u64,
            ..Self::new()
        }
    }

    /// Peeks without releasing FIFO space until the external transport accepts it.
    pub fn pending_transmit(&self) -> Option<u8> {
        (self.transmit_fifo.length != 0)
            .then(|| self.transmit_fifo.entries[self.transmit_fifo.head] as u8)
    }

    /// Reports whether the external byte stream may supply another byte.
    pub fn can_receive(&self) -> bool {
        !self.receive_fifo.is_full()
    }

    /// Reports whether a guest DR write can complete without dropping data.
    pub fn can_transmit(&self) -> bool {
        !self.transmit_fifo.is_full()
    }

    /// Accepts one byte or reports overrun, preserving all already queued data.
    /// The owner normally checks `can_receive` to backpressure its transport.
    pub fn receive(&mut self, byte: u8, now: u64) -> bool {
        self.advance(now);
        let old_length = self.receive_fifo.length;
        // PL011 FIFO semantics attach a pending overrun marker to the first
        // character accepted after overflow, not to already queued characters.
        let entry = u16::from(byte)
            | if self.data_overrun {
                reg::DR_OE as u16
            } else {
                0
            };
        let accepted = self.receive_fifo.push(entry);
        if accepted {
            self.data_overrun = false;
            if old_length < FIFO_TRIGGER && self.receive_fifo.length >= FIFO_TRIGGER {
                self.interrupt_status |= reg::INT_RX;
            }
        } else {
            self.data_overrun = true;
            self.receive_status |= reg::RSR_OE;
            self.interrupt_status |= reg::INT_OE;
        }
        // Incoming data restarts the idle interval, but an already pending
        // timeout stays latched until ICR or an empty receive FIFO clears it.
        self.restart_timeout(now);
        accepted
    }

    /// Completes a byte only when the owner has room in its output transport.
    /// No simulated baud delay is inserted in this virtual byte stream.
    pub fn take_transmitted(&mut self) -> Option<u8> {
        let old_length = self.transmit_fifo.length;
        let byte = self.transmit_fifo.pop()?;
        let length = self.transmit_fifo.length;
        // A short burst can empty without ever exceeding the trigger. It still
        // completes a transmit event; merely enabling TX interrupts does not.
        if (old_length > FIFO_TRIGGER && length <= FIFO_TRIGGER) || length == 0 {
            self.interrupt_status |= reg::INT_TX;
        }
        Some(byte as u8)
    }

    /// Updates the 32-bit-period receive timer using the owner's monotonic ticks.
    pub fn advance(&mut self, now: u64) {
        if self
            .receive_deadline
            .is_some_and(|deadline| now.wrapping_sub(deadline) < (1 << 63))
        {
            self.receive_deadline = None;
            if self.receive_fifo.length != 0 {
                self.interrupt_status |= reg::INT_RT;
            }
        }
    }

    pub fn next_timeout(&self) -> Option<u64> {
        self.receive_deadline
    }

    pub fn interrupt_asserted(&self) -> bool {
        self.interrupt_status & self.interrupt_mask != 0
    }

    /// Reads at a register's base address, including its FIFO side effects.
    /// Byte reads are a supported extension for registers whose SBSA minimum
    /// access width is 16 bits; all reads are truncated to the requested width.
    pub fn read(&mut self, offset: u64, width: usize) -> Result<u64, AccessError> {
        let mask = access_mask(offset, width)?;
        let value = match offset {
            reg::DR => self.read_data(),
            reg::RSR_ECR => self.receive_status,
            reg::FR => self.flags(),
            reg::IMSC => self.interrupt_mask,
            reg::RIS => self.interrupt_status,
            reg::MIS => self.interrupt_status & self.interrupt_mask,
            // Reserved registers and reads of the write-only ICR are RAZ.
            _ => 0,
        };
        Ok(u64::from(value & mask))
    }

    /// Writes a register without retaining references to guest memory.
    /// Writes to read-only or reserved registers are ignored. A full transmit
    /// FIFO returns `TransmitFull` without changing any queued character.
    pub fn write(
        &mut self,
        offset: u64,
        width: usize,
        value: u64,
        now: u64,
    ) -> Result<(), AccessError> {
        let mask = access_mask(offset, width)?;
        let value = value as u32 & mask;
        self.advance(now);
        match offset {
            reg::DR => {
                if !self.transmit_fifo.push(u16::from(value as u8)) {
                    return Err(AccessError::TransmitFull);
                }
                if self.transmit_fifo.length > FIFO_TRIGGER {
                    self.interrupt_status &= !reg::INT_TX;
                }
            }
            reg::RSR_ECR => {
                // Clear receive error state, including overflow awaiting its
                // next accepted byte. Already queued markers stay with their
                // entries; ICR instead clears only the interrupt latch.
                self.receive_status = 0;
                self.data_overrun = false;
            }
            reg::IMSC => self.write_interrupt_mask(value, now),
            reg::ICR => {
                self.interrupt_status &= !(value & reg::INT_ALL);
                if value & reg::INT_RT != 0 {
                    self.restart_timeout(now);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn read_data(&mut self) -> u32 {
        let Some(entry) = self.receive_fifo.pop() else {
            return 0;
        };
        if self.receive_fifo.length < FIFO_TRIGGER {
            self.interrupt_status &= !reg::INT_RX;
        }
        if self.receive_fifo.length == 0 {
            self.interrupt_status &= !reg::INT_RT;
            self.receive_deadline = None;
        }
        u32::from(entry)
    }

    fn write_interrupt_mask(&mut self, value: u32, now: u64) {
        let old_mask = self.interrupt_mask;
        // A set mask bit enables its interrupt, as in the PL011 subset.
        self.interrupt_mask = value & reg::INT_ALL;
        // RTIM also gates the timer itself (DDI0183G, table 3-15 footnote a).
        // Disable drops the pending timeout; reenable starts a fresh interval.
        if self.interrupt_mask & reg::INT_RT == 0 {
            self.interrupt_status &= !reg::INT_RT;
            self.receive_deadline = None;
        } else if old_mask & reg::INT_RT == 0 {
            self.restart_timeout(now);
        }
    }

    fn restart_timeout(&mut self, now: u64) {
        self.receive_deadline = if self.receive_fifo.length != 0
            && self.interrupt_mask & reg::INT_RT != 0
            && self.interrupt_status & reg::INT_RT == 0
        {
            Some(now.wrapping_add(self.receive_timeout_ticks))
        } else {
            None
        };
    }

    fn flags(&self) -> u32 {
        let mut flags = 0;
        if self.receive_fifo.length == 0 {
            flags |= reg::FR_RXFE;
        }
        if self.receive_fifo.is_full() {
            flags |= reg::FR_RXFF;
        }
        if self.transmit_fifo.length == 0 {
            flags |= reg::FR_TXFE;
        } else {
            flags |= reg::FR_BUSY;
        }
        if self.transmit_fifo.is_full() {
            flags |= reg::FR_TXFF;
        }
        flags
    }
}

struct Fifo {
    entries: [u16; FIFO_CAPACITY],
    head: usize,
    length: usize,
}

impl Fifo {
    const fn new() -> Self {
        Self {
            entries: [0; FIFO_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    fn is_full(&self) -> bool {
        self.length == FIFO_CAPACITY
    }

    fn push(&mut self, entry: u16) -> bool {
        if self.is_full() {
            return false;
        }
        self.entries[(self.head + self.length) % FIFO_CAPACITY] = entry;
        self.length += 1;
        true
    }

    fn pop(&mut self) -> Option<u16> {
        if self.length == 0 {
            return None;
        }
        let entry = self.entries[self.head];
        self.head = (self.head + 1) % FIFO_CAPACITY;
        self.length -= 1;
        Some(entry)
    }
}

fn access_mask(offset: u64, width: usize) -> Result<u32, AccessError> {
    if offset >= REGISTER_FRAME_BYTES || offset & 3 != 0 {
        return Err(AccessError::InvalidAccess);
    }
    match width {
        1 => Ok(0xff),
        2 => Ok(0xffff),
        4 => Ok(u32::MAX),
        _ => Err(AccessError::InvalidAccess),
    }
}
