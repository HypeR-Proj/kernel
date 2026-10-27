// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free 16550A register model with instantaneous transmission.
//! The owner schedules `next_timeout` once and calls `advance` on expiry.

pub struct Ns16550 {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scratch: u8,
    divisor: [u8; 2],
    fcr: u8,
    rx: [u8; 16],
    head: usize,
    len: usize,
    overrun: bool,
    tx_pending: bool,
    modem_delta: u8,
    tick_frequency: u64,
    uart_clock: u64,
    deadline: Option<u64>,
    timeout_pending: bool,
}
impl Default for Ns16550 {
    fn default() -> Self {
        Self::new()
    }
}
impl Ns16550 {
    pub const fn new() -> Self {
        Self::with_clock(1, 3_686_400)
    }
    pub const fn with_clock(tick_frequency: u64, uart_clock: u64) -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scratch: 0,
            divisor: [0; 2],
            fcr: 0,
            rx: [0; 16],
            head: 0,
            len: 0,
            overrun: false,
            tx_pending: false,
            modem_delta: 0,
            tick_frequency,
            uart_clock,
            deadline: None,
            timeout_pending: false,
        }
    }
    pub fn read(&mut self, register: usize) -> u8 {
        self.read_at(register, 0)
    }
    pub fn write(&mut self, register: usize, value: u8) -> Option<u8> {
        self.write_at(register, value, 0)
    }
    fn fifo(&self) -> bool {
        self.fcr & 1 != 0
    }
    fn trigger(&self) -> usize {
        if self.fifo() {
            [1, 4, 8, 14][usize::from(self.fcr >> 6)]
        } else {
            1
        }
    }
    pub fn can_receive(&self) -> bool {
        self.len < if self.fifo() { 16 } else { 1 }
    }
    /// External input is disconnected while the internal loopback path owns RX.
    pub fn can_receive_external(&self) -> bool {
        self.mcr & 16 == 0 && self.can_receive()
    }
    pub fn receive_external(&mut self, byte: u8, now: u64) -> bool {
        if self.mcr & 16 != 0 {
            return false;
        }
        self.receive(byte, now)
    }
    fn restart_timeout(&mut self, now: u64) {
        self.timeout_pending = false;
        self.deadline = None;
        if !self.fifo() || self.len == 0 || self.uart_clock == 0 {
            return;
        }
        let divisor = u64::from(u16::from_le_bytes(self.divisor)).max(1);
        // Half-bit units cover the 5-bit/1.5-stop-bit configuration exactly.
        let data = u64::from(self.lcr & 3) + 5;
        let stop = if self.lcr & 4 == 0 {
            2
        } else if data == 5 {
            3
        } else {
            4
        };
        let half_bits = 2 + data * 2 + if self.lcr & 8 != 0 { 2 } else { 0 } + stop;
        let numerator =
            u128::from(self.tick_frequency) * 16 * u128::from(divisor) * u128::from(half_bits) * 2;
        let ticks = numerator
            .div_ceil(u128::from(self.uart_clock))
            .clamp(1, i64::MAX as u128) as u64;
        self.deadline = Some(now.wrapping_add(ticks));
    }
    pub fn advance(&mut self, now: u64) {
        if self
            .deadline
            .is_some_and(|deadline| now.wrapping_sub(deadline) < (1 << 63))
        {
            self.deadline = None;
            self.timeout_pending = self.len != 0;
        }
    }
    pub fn next_timeout(&self) -> Option<u64> {
        self.deadline
    }
    pub fn receive(&mut self, byte: u8, now: u64) -> bool {
        if !self.can_receive() {
            self.overrun = true;
            return false;
        }
        self.rx[(self.head + self.len) % 16] = byte;
        self.len += 1;
        self.restart_timeout(now);
        true
    }
    fn interrupt_id(&self) -> u8 {
        if self.ier & 4 != 0 && self.overrun {
            6
        } else if self.ier & 1 != 0 && self.len >= self.trigger() {
            4
        } else if self.ier & 1 != 0 && self.timeout_pending {
            12
        } else if self.ier & 2 != 0 && self.tx_pending {
            2
        } else if self.ier & 8 != 0 && self.modem_delta != 0 {
            0
        } else {
            1
        }
    }
    pub fn interrupt_asserted(&self) -> bool {
        self.interrupt_id() != 1
    }
    fn modem(&self) -> u8 {
        if self.mcr & 16 == 0 {
            0xb0
        } else {
            ((self.mcr & 2) << 3)
                | ((self.mcr & 1) << 5)
                | ((self.mcr & 4) << 4)
                | ((self.mcr & 8) << 4)
        }
    }
    pub fn read_at(&mut self, register: usize, now: u64) -> u8 {
        self.advance(now);
        match register {
            0 if self.lcr & 0x80 != 0 => self.divisor[0],
            0 => {
                let byte = if self.len == 0 {
                    0
                } else {
                    let b = self.rx[self.head];
                    self.head = (self.head + 1) % 16;
                    self.len -= 1;
                    b
                };
                self.restart_timeout(now);
                byte
            }
            1 if self.lcr & 0x80 != 0 => self.divisor[1],
            1 => self.ier,
            2 => {
                let id = self.interrupt_id();
                if id == 2 {
                    self.tx_pending = false;
                }
                id | if self.fifo() { 0xc0 } else { 0 }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                let value = 0x60 | u8::from(self.len != 0) | (u8::from(self.overrun) << 1);
                self.overrun = false;
                value
            }
            6 => {
                let value = self.modem() | self.modem_delta;
                self.modem_delta = 0;
                value
            }
            7 => self.scratch,
            _ => 0,
        }
    }
    pub fn write_at(&mut self, register: usize, value: u8, now: u64) -> Option<u8> {
        self.advance(now);
        match register {
            0 if self.lcr & 0x80 != 0 => {
                self.divisor[0] = value;
                self.restart_timeout(now);
            }
            0 => {
                self.tx_pending = self.ier & 2 != 0;
                if self.mcr & 16 != 0 {
                    self.receive(value, now);
                } else {
                    return Some(value);
                }
            }
            1 if self.lcr & 0x80 != 0 => {
                self.divisor[1] = value;
                self.restart_timeout(now);
            }
            1 => {
                let was = self.ier & 2 != 0;
                self.ier = value & 15;
                self.tx_pending = self.ier & 2 != 0 && (!was || self.tx_pending);
            }
            2 => {
                let reset = (self.fcr ^ value) & 1 != 0 || value & 2 != 0;
                self.fcr = value & 0xc1;
                if reset {
                    self.head = 0;
                    self.len = 0;
                }
                self.restart_timeout(now);
            }
            3 => {
                self.lcr = value;
                self.restart_timeout(now);
            }
            4 => {
                let old = self.modem();
                self.mcr = value & 31;
                let new = self.modem();
                self.modem_delta |= ((old ^ new) >> 4) & 0xb;
                if old & 0x40 != 0 && new & 0x40 == 0 {
                    self.modem_delta |= 4;
                }
            }
            7 => self.scratch = value,
            _ => {}
        }
        None
    }
}
