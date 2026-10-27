// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! NS16550-compatible UART register model and controller operations.
//!
//! This facade owns hardware-independent UART semantics and delegates physical
//! register access to `access`. Platform discovery and driver-instance
//! lifecycle live in `platform_binding`; neither lower layer chooses kernel
//! console or IRQ policy.

mod access;
mod platform_binding;

use crate::platform::PermanentMmioMapping;
use hyper::hal::console::Console;
use hyper::hal::io::PortIo;

pub use access::MmioAccess;
use access::{Register, RegisterBank};
pub use platform_binding::PLATFORM_DRIVER;

const IIR_NO_INTERRUPT: u8 = 1;
const IIR_IDENTIFICATION_MASK: u8 = 0x0e;
const FCR_ENABLE: u8 = 1;
const FCR_CLEAR_RECEIVE: u8 = 1 << 1;
const FCR_CLEAR_TRANSMIT: u8 = 1 << 2;
const FCR_TRIGGER_SHIFT: u8 = 6;
const LCR_STOP: u8 = 1 << 2;
const LCR_PARITY_ENABLE: u8 = 1 << 3;
const LCR_EVEN_PARITY: u8 = 1 << 4;
const LCR_STICK_PARITY: u8 = 1 << 5;
const LCR_BREAK: u8 = 1 << 6;
const LCR_DLAB: u8 = 1 << 7;
const MCR_DTR: u8 = 1;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT1: u8 = 1 << 2;
const MCR_OUT2: u8 = 1 << 3;
const MCR_LOOPBACK: u8 = 1 << 4;
const LSR_DATA_READY: u8 = 1;
const LSR_OVERRUN_ERROR: u8 = 1 << 1;
const LSR_PARITY_ERROR: u8 = 1 << 2;
const LSR_FRAMING_ERROR: u8 = 1 << 3;
const LSR_BREAK_INTERRUPT: u8 = 1 << 4;
const LSR_THR_EMPTY: u8 = 1 << 5;
const LSR_TRANSMITTER_EMPTY: u8 = 1 << 6;
const LSR_FIFO_ERROR: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    BaudRateOutOfRange,
    InvalidAccess,
    InvalidClock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopBits {
    One,
    Two,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Parity {
    None,
    Odd,
    Even,
    StickOne,
    StickZero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineConfig {
    pub data_bits: DataBits,
    pub stop_bits: StopBits,
    pub parity: Parity,
}

impl LineConfig {
    pub const EIGHT_N_ONE: Self = Self {
        data_bits: DataBits::Eight,
        stop_bits: StopBits::One,
        parity: Parity::None,
    };

    const fn register_value(self) -> u8 {
        let data = match self.data_bits {
            DataBits::Five => 0,
            DataBits::Six => 1,
            DataBits::Seven => 2,
            DataBits::Eight => 3,
        };
        let stop = match self.stop_bits {
            StopBits::One => 0,
            StopBits::Two => LCR_STOP,
        };
        let parity = match self.parity {
            Parity::None => 0,
            Parity::Odd => LCR_PARITY_ENABLE,
            Parity::Even => LCR_PARITY_ENABLE | LCR_EVEN_PARITY,
            Parity::StickOne => LCR_PARITY_ENABLE | LCR_STICK_PARITY,
            Parity::StickZero => LCR_PARITY_ENABLE | LCR_EVEN_PARITY | LCR_STICK_PARITY,
        };
        data | stop | parity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FifoTrigger {
    OneByte = 0,
    FourBytes = 1,
    EightBytes = 2,
    FourteenBytes = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptMask(u8);

impl InterruptMask {
    pub const NONE: Self = Self(0);
    pub const RECEIVED_DATA: Self = Self(1);
    pub const TRANSMIT_EMPTY: Self = Self(1 << 1);
    pub const LINE_STATUS: Self = Self(1 << 2);
    pub const MODEM_STATUS: Self = Self(1 << 3);
    pub const RUNTIME_INPUT: Self = Self(Self::RECEIVED_DATA.0 | Self::LINE_STATUS.0);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn raw(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptIdentification {
    None,
    ModemStatus,
    TransmitEmpty,
    ReceivedData,
    LineStatus,
    CharacterTimeout,
    Unknown(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedByte {
    pub byte: u8,
    pub overrun_error: bool,
    pub parity_error: bool,
    pub framing_error: bool,
    pub break_interrupt: bool,
    pub fifo_error: bool,
}

impl ReceivedByte {
    pub const fn has_error(self) -> bool {
        self.overrun_error
            || self.parity_error
            || self.framing_error
            || self.break_interrupt
            || self.fifo_error
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModemStatus(u8);

impl ModemStatus {
    pub const fn raw(self) -> u8 {
        self.0
    }

    pub const fn clear_to_send(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    pub const fn data_set_ready(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    pub const fn ring_indicator(self) -> bool {
        self.0 & (1 << 6) != 0
    }

    pub const fn carrier_detect(self) -> bool {
        self.0 & (1 << 7) != 0
    }
}

/// Shareable handle for one permanently mapped NS16550 register bank.
#[derive(Clone, Copy)]
pub struct Ns16550 {
    registers: RegisterBank,
}

impl Ns16550 {
    /// Binds an NS16550-compatible UART to a permanent register mapping.
    pub fn from_mapped_mmio(
        mapping: PermanentMmioMapping,
        access: MmioAccess,
    ) -> Result<Self, Error> {
        Ok(Self {
            registers: RegisterBank::from_mapped_mmio(mapping, access)?,
        })
    }

    /// Creates a handle for an explicitly described register interface.
    ///
    /// # Safety
    ///
    /// The complete register window must be mapped with device attributes and
    /// register ownership must be externally serialized.
    pub(crate) const unsafe fn from_mmio(base: usize, access: MmioAccess) -> Result<Self, Error> {
        // SAFETY: The caller supplies the complete mapping and ownership
        // contract, which the access backend validates and retains.
        match unsafe { RegisterBank::from_mmio(base, access) } {
            Ok(registers) => Ok(Self { registers }),
            Err(error) => Err(error),
        }
    }

    /// Creates a handle for an isolated I/O-port register bank.
    ///
    /// # Safety
    ///
    /// The caller must own all eight consecutive ports beginning at `base`.
    pub const unsafe fn from_port(base: u16, io: PortIo) -> Result<Self, Error> {
        // SAFETY: The caller's complete eight-port ownership contract is
        // forwarded unchanged to the isolated access backend.
        match unsafe { RegisterBank::from_port(base, io) } {
            Ok(registers) => Ok(Self { registers }),
            Err(error) => Err(error),
        }
    }

    pub(crate) const fn mmio_base(self) -> usize {
        self.registers.mmio_base()
    }

    pub(crate) const fn mmio_access(self) -> Option<MmioAccess> {
        self.registers.mmio_access()
    }

    pub fn configure(
        &self,
        input_clock_hz: u32,
        baud_rate: u32,
        line: LineConfig,
        receive_trigger: FifoTrigger,
    ) -> Result<(), Error> {
        let divisor = baud_divisor(input_clock_hz, baud_rate)?;
        self.set_interrupt_mask(InterruptMask::NONE);
        self.write(Register::LINE_CONTROL, LCR_DLAB);
        self.write(Register::DATA, divisor as u8);
        self.write(Register::INTERRUPT_ENABLE, (divisor >> 8) as u8);
        self.write(Register::LINE_CONTROL, line.register_value());
        self.configure_fifo(true, receive_trigger, true, true);
        self.set_modem_outputs(true, true, false, true, false);
        self.clear_pending_state();
        Ok(())
    }

    /// Takes input ownership while preserving firmware baud and line format.
    pub fn enable_runtime_input(&self, receive_trigger: FifoTrigger) {
        self.set_interrupt_mask(InterruptMask::NONE);
        self.configure_fifo(true, receive_trigger, true, false);
        self.clear_pending_state();
        self.set_irq_output(true);
        self.set_interrupt_mask(InterruptMask::RUNTIME_INPUT);
    }

    pub fn set_interrupt_mask(&self, mask: InterruptMask) {
        let line = self.read(Register::LINE_CONTROL);
        self.write(Register::LINE_CONTROL, line & !LCR_DLAB);
        self.write(Register::INTERRUPT_ENABLE, mask.raw() & 0x0f);
        self.write(Register::LINE_CONTROL, line & !LCR_DLAB);
    }

    pub fn interrupt_identification(&self) -> InterruptIdentification {
        let value = self.read(Register::INTERRUPT_IDENTIFICATION);
        if value & IIR_NO_INTERRUPT != 0 {
            return InterruptIdentification::None;
        }
        match value & IIR_IDENTIFICATION_MASK {
            0x00 => InterruptIdentification::ModemStatus,
            0x02 => InterruptIdentification::TransmitEmpty,
            0x04 => InterruptIdentification::ReceivedData,
            0x06 => InterruptIdentification::LineStatus,
            0x0c => InterruptIdentification::CharacterTimeout,
            other => InterruptIdentification::Unknown(other),
        }
    }

    pub fn configure_fifo(
        &self,
        enabled: bool,
        receive_trigger: FifoTrigger,
        clear_receive: bool,
        clear_transmit: bool,
    ) {
        self.write(
            Register::INTERRUPT_IDENTIFICATION,
            if enabled { FCR_ENABLE } else { 0 }
                | if clear_receive { FCR_CLEAR_RECEIVE } else { 0 }
                | if clear_transmit {
                    FCR_CLEAR_TRANSMIT
                } else {
                    0
                }
                | ((receive_trigger as u8) << FCR_TRIGGER_SHIFT),
        );
    }

    pub fn try_read(&self) -> Option<ReceivedByte> {
        let status = self.read(Register::LINE_STATUS);
        if status & LSR_DATA_READY == 0 {
            return None;
        }
        Some(ReceivedByte {
            byte: self.read(Register::DATA),
            overrun_error: status & LSR_OVERRUN_ERROR != 0,
            parity_error: status & LSR_PARITY_ERROR != 0,
            framing_error: status & LSR_FRAMING_ERROR != 0,
            break_interrupt: status & LSR_BREAK_INTERRUPT != 0,
            fifo_error: status & LSR_FIFO_ERROR != 0,
        })
    }

    pub fn try_write(&self, byte: u8) -> bool {
        if self.read(Register::LINE_STATUS) & LSR_THR_EMPTY == 0 {
            return false;
        }
        self.write(Register::DATA, byte);
        true
    }

    pub fn wait_until_transmitted(&self) {
        while self.read(Register::LINE_STATUS) & LSR_TRANSMITTER_EMPTY == 0 {
            core::hint::spin_loop();
        }
    }

    pub fn set_break(&self, enabled: bool) {
        let mut control = self.read(Register::LINE_CONTROL);
        if enabled {
            control |= LCR_BREAK;
        } else {
            control &= !LCR_BREAK;
        }
        self.write(Register::LINE_CONTROL, control);
    }

    pub fn set_modem_outputs(&self, dtr: bool, rts: bool, out1: bool, out2: bool, loopback: bool) {
        self.write(
            Register::MODEM_CONTROL,
            if dtr { MCR_DTR } else { 0 }
                | if rts { MCR_RTS } else { 0 }
                | if out1 { MCR_OUT1 } else { 0 }
                | if out2 { MCR_OUT2 } else { 0 }
                | if loopback { MCR_LOOPBACK } else { 0 },
        );
    }

    /// Controls the conventional OUT2 interrupt gate without changing other
    /// modem outputs. Platforms that do not implement the gate ignore it.
    pub fn set_irq_output(&self, enabled: bool) {
        let control = self.read(Register::MODEM_CONTROL);
        self.write(
            Register::MODEM_CONTROL,
            if enabled {
                control | MCR_OUT2
            } else {
                control & !MCR_OUT2
            },
        );
    }

    pub fn modem_status(&self) -> ModemStatus {
        ModemStatus(self.read(Register::MODEM_STATUS))
    }

    pub fn write_scratch(&self, value: u8) {
        self.write(Register::SCRATCH, value);
    }

    pub fn read_scratch(&self) -> u8 {
        self.read(Register::SCRATCH)
    }

    fn clear_pending_state(&self) {
        let _ = self.read(Register::LINE_STATUS);
        let _ = self.read(Register::DATA);
        let _ = self.read(Register::INTERRUPT_IDENTIFICATION);
        let _ = self.read(Register::MODEM_STATUS);
    }

    fn read(&self, register: Register) -> u8 {
        self.registers.read(register)
    }

    fn write(&self, register: Register, value: u8) {
        self.registers.write(register, value);
    }
}

impl Console for Ns16550 {
    fn try_write_byte(&self, byte: u8) -> bool {
        self.try_write(byte)
    }
}

fn baud_divisor(input_clock_hz: u32, baud_rate: u32) -> Result<u16, Error> {
    if input_clock_hz == 0 {
        return Err(Error::InvalidClock);
    }
    if baud_rate == 0 {
        return Err(Error::BaudRateOutOfRange);
    }
    let denominator = u64::from(baud_rate) * 16;
    let divisor = (u64::from(input_clock_hz) + denominator / 2) / denominator;
    u16::try_from(divisor)
        .ok()
        .filter(|divisor| *divisor != 0)
        .ok_or(Error::BaudRateOutOfRange)
}
