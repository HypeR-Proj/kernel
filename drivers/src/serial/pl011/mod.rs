// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! ARM `PrimeCell` PL011 serial controller driver.

use alloc::boxed::Box;
use core::{
    mem::align_of,
    ptr::{read_volatile, write_volatile},
};

use hyper::hal::console::Console;
use {
    crate::platform::DriverInstance, crate::platform::DriverServices,
    crate::platform::MmioMappingError, crate::platform::PermanentMmioMapping,
    crate::platform::PlatformDevice, crate::platform::PlatformDriver, crate::platform::ProbeError,
};

use hyper::hw::pl011 as reg;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BaudRateOutOfRange,
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
pub enum Parity {
    None,
    Odd,
    Even,
    StickOne,
    StickZero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopBits {
    One,
    Two,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineConfig {
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub fifo: bool,
}

impl LineConfig {
    pub const EIGHT_N_ONE: Self = Self {
        data_bits: DataBits::Eight,
        parity: Parity::None,
        stop_bits: StopBits::One,
        fifo: true,
    };

    const fn register_value(self) -> u32 {
        let word_length = match self.data_bits {
            DataBits::Five => 0,
            DataBits::Six => 1,
            DataBits::Seven => 2,
            DataBits::Eight => 3,
        } << reg::LCR_H_WLEN_SHIFT;
        let parity = match self.parity {
            Parity::None => 0,
            Parity::Odd => reg::LCR_H_PEN,
            Parity::Even => reg::LCR_H_PEN | reg::LCR_H_EPS,
            Parity::StickOne => reg::LCR_H_PEN | reg::LCR_H_SPS,
            Parity::StickZero => reg::LCR_H_PEN | reg::LCR_H_EPS | reg::LCR_H_SPS,
        };
        word_length
            | parity
            | match self.stop_bits {
                StopBits::One => 0,
                StopBits::Two => reg::LCR_H_STP2,
            }
            | if self.fifo { reg::LCR_H_FEN } else { 0 }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum FifoLevel {
    OneEighth = 0,
    OneQuarter = 1,
    OneHalf = 2,
    ThreeQuarters = 3,
    SevenEighths = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedByte {
    pub byte: u8,
    pub framing_error: bool,
    pub parity_error: bool,
    pub break_error: bool,
    pub overrun_error: bool,
}

impl ReceivedByte {
    pub const EMPTY: Self = Self {
        byte: 0,
        framing_error: false,
        parity_error: false,
        break_error: false,
        overrun_error: false,
    };

    const fn from_data(value: u32) -> Self {
        Self {
            byte: (value & reg::DR_DATA_MASK) as u8,
            framing_error: value & reg::DR_FE != 0,
            parity_error: value & reg::DR_PE != 0,
            break_error: value & reg::DR_BE != 0,
            overrun_error: value & reg::DR_OE != 0,
        }
    }

    pub const fn has_error(self) -> bool {
        self.framing_error || self.parity_error || self.break_error || self.overrun_error
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptStatus(u32);

impl InterruptStatus {
    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn contains(self, mask: u32) -> bool {
        self.0 & mask != 0
    }
}

/// MMIO access to one physical Arm `PrimeCell` PL011 UART.
///
/// The value is a shareable handle rather than exclusive mutable state. The
/// console and serial-input layers may each retain a handle, but must coordinate
/// reconfiguration and interrupt ownership at the subsystem boundary.
#[derive(Clone, Copy)]
pub struct Pl011 {
    base: usize,
    has_identification_registers: bool,
}

impl Pl011 {
    /// Operational registers, excluding optional `PrimeCell` identification.
    pub const REGISTER_WINDOW_SIZE: u64 = (reg::TDR + 4) as u64;

    /// Binds a PL011 to a permanent, validated register mapping.
    pub fn from_mapped_mmio(mapping: PermanentMmioMapping) -> Result<Self, MmioMappingError> {
        let mapping = mapping.validate_window(Self::REGISTER_WINDOW_SIZE, align_of::<u32>())?;
        Ok(Self {
            base: mapping.virtual_start(),
            has_identification_registers: mapping.resource().size() >= 0x1000,
        })
    }

    /// # Safety
    ///
    /// `base` must identify a mapped PL011 register block for the lifetime of
    /// every copied handle. Register ownership must be coordinated by callers.
    pub(crate) const unsafe fn from_mmio_base(base: usize) -> Self {
        // Early/emergency consoles only access the operational registers.
        Self {
            base,
            has_identification_registers: false,
        }
    }

    pub(crate) const fn mmio_base(self) -> usize {
        self.base
    }

    pub fn configure(
        &self,
        input_clock_hz: u32,
        baud_rate: u32,
        line: LineConfig,
    ) -> Result<(), Error> {
        let (integer, fractional) = baud_divisors(input_clock_hz, baud_rate)?;
        self.set_interrupt_mask(0);
        self.write_register(reg::CR, 0);
        self.wait_until_idle();
        self.write_register(reg::ICR, reg::INT_ALL);
        self.write_register(reg::IBRD, integer);
        self.write_register(reg::FBRD, fractional);
        self.write_register(reg::LCR_H, line.register_value());
        self.write_register(reg::CR, reg::CR_UARTEN | reg::CR_TXE | reg::CR_RXE);
        Ok(())
    }

    /// Takes runtime ownership while preserving firmware baud and line setup.
    pub fn enable_runtime_input(&self) {
        self.set_interrupt_mask(0);
        self.clear_receive_errors();
        self.write_register(reg::ICR, reg::INT_ALL);
        self.set_fifo_levels(FifoLevel::OneEighth, FifoLevel::OneEighth);
        let control = self.read_register(reg::CR);
        self.write_register(
            reg::CR,
            control | reg::CR_UARTEN | reg::CR_TXE | reg::CR_RXE,
        );
        self.set_interrupt_mask(reg::INT_RECEIVE_MASK | reg::INT_ERROR_MASK);
    }

    pub fn disable_interrupts(&self) {
        self.set_interrupt_mask(0);
        self.write_register(reg::ICR, reg::INT_ALL);
    }

    pub fn set_interrupt_mask(&self, mask: u32) {
        self.write_register(reg::IMSC, mask & reg::INT_ALL);
    }

    pub fn enable_interrupts(&self, mask: u32) {
        self.set_interrupt_mask(self.read_register(reg::IMSC) | mask);
    }

    pub fn disable_interrupt_sources(&self, mask: u32) {
        self.set_interrupt_mask(self.read_register(reg::IMSC) & !mask);
    }

    pub fn raw_interrupt_status(&self) -> InterruptStatus {
        InterruptStatus(self.read_register(reg::RIS) & reg::INT_ALL)
    }

    pub fn masked_interrupt_status(&self) -> InterruptStatus {
        InterruptStatus(self.read_register(reg::MIS) & reg::INT_ALL)
    }

    pub fn clear_interrupts(&self, mask: u32) {
        self.write_register(reg::ICR, mask & reg::INT_ALL);
    }

    pub fn try_read(&self) -> Option<ReceivedByte> {
        if self.read_register(reg::FR) & reg::FR_RXFE != 0 {
            return None;
        }
        Some(ReceivedByte::from_data(self.read_register(reg::DR)))
    }

    pub fn try_write(&self, byte: u8) -> bool {
        if self.read_register(reg::FR) & reg::FR_TXFF != 0 {
            return false;
        }
        self.write_register(reg::DR, u32::from(byte));
        true
    }

    pub fn wait_until_idle(&self) {
        while self.read_register(reg::FR) & reg::FR_BUSY != 0 {
            core::hint::spin_loop();
        }
    }

    pub fn clear_receive_errors(&self) {
        self.write_register(reg::RSR_ECR, reg::RSR_ERROR_MASK);
    }

    pub fn set_fifo_levels(&self, transmit: FifoLevel, receive: FifoLevel) {
        self.write_register(
            reg::IFLS,
            (transmit as u32) << reg::IFLS_TX_SHIFT | (receive as u32) << reg::IFLS_RX_SHIFT,
        );
    }

    pub fn set_dma(&self, receive: bool, transmit: bool, continue_on_error: bool) {
        self.write_register(
            reg::DMACR,
            if receive { reg::DMACR_RXDMAE } else { 0 }
                | if transmit { reg::DMACR_TXDMAE } else { 0 }
                | if continue_on_error {
                    reg::DMACR_DMAONERR
                } else {
                    0
                },
        );
    }

    pub fn set_hardware_flow_control(&self, receive_cts: bool, transmit_rts: bool) {
        let mut control = self.read_register(reg::CR) & !(reg::CR_CTSEN | reg::CR_RTSEN);
        if receive_cts {
            control |= reg::CR_CTSEN;
        }
        if transmit_rts {
            control |= reg::CR_RTSEN;
        }
        self.write_register(reg::CR, control);
    }

    pub fn set_modem_outputs(&self, dtr: bool, rts: bool, out1: bool, out2: bool) {
        let mask = reg::CR_DTR | reg::CR_RTS | reg::CR_OUT1 | reg::CR_OUT2;
        let outputs = if dtr { reg::CR_DTR } else { 0 }
            | if rts { reg::CR_RTS } else { 0 }
            | if out1 { reg::CR_OUT1 } else { 0 }
            | if out2 { reg::CR_OUT2 } else { 0 };
        self.write_register(reg::CR, self.read_register(reg::CR) & !mask | outputs);
    }

    pub fn set_loopback(&self, enabled: bool) {
        self.update_control_bit(reg::CR_LBE, enabled);
    }

    pub fn set_break(&self, enabled: bool) {
        let line = self.read_register(reg::LCR_H);
        self.write_register(
            reg::LCR_H,
            if enabled {
                line | reg::LCR_H_BRK
            } else {
                line & !reg::LCR_H_BRK
            },
        );
    }

    pub fn set_irda(&self, enabled: bool, low_power: bool, divisor: u8) {
        self.write_register(reg::ILPR, u32::from(divisor));
        let mut control = self.read_register(reg::CR) & !(reg::CR_SIREN | reg::CR_SIRLP);
        if enabled {
            control |= reg::CR_SIREN;
        }
        if low_power {
            control |= reg::CR_SIRLP;
        }
        self.write_register(reg::CR, control);
    }

    pub fn modem_status(&self) -> u32 {
        self.read_register(reg::FR) & (reg::FR_CTS | reg::FR_DSR | reg::FR_DCD | reg::FR_RI)
    }

    /// AXI integrations such as BCM2712 expose only the operational window.
    /// Never probe `PrimeCell` ID offsets outside the firmware-described resource.
    pub fn peripheral_id(&self) -> Option<[u8; 8]> {
        if !self.has_identification_registers {
            return None;
        }
        Some([
            self.read_register(reg::PERIPH_ID0) as u8,
            self.read_register(reg::PERIPH_ID1) as u8,
            self.read_register(reg::PERIPH_ID2) as u8,
            self.read_register(reg::PERIPH_ID3) as u8,
            self.read_register(reg::PCELL_ID0) as u8,
            self.read_register(reg::PCELL_ID1) as u8,
            self.read_register(reg::PCELL_ID2) as u8,
            self.read_register(reg::PCELL_ID3) as u8,
        ])
    }

    fn update_control_bit(&self, bit: u32, enabled: bool) {
        let control = self.read_register(reg::CR);
        self.write_register(
            reg::CR,
            if enabled {
                control | bit
            } else {
                control & !bit
            },
        );
    }

    fn read_register(&self, offset: usize) -> u32 {
        // SAFETY: The constructor requires a valid, permanently mapped PL011.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write_register(&self, offset: usize, value: u32) {
        // SAFETY: The constructor requires a valid, permanently mapped PL011.
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }
}

fn baud_divisors(input_clock_hz: u32, baud_rate: u32) -> Result<(u32, u32), Error> {
    if input_clock_hz == 0 || baud_rate == 0 {
        return Err(Error::InvalidClock);
    }
    let numerator = u64::from(input_clock_hz)
        .checked_mul(4)
        .ok_or(Error::BaudRateOutOfRange)?;
    let divisor64 = numerator
        .checked_add(u64::from(baud_rate) / 2)
        .ok_or(Error::BaudRateOutOfRange)?
        / u64::from(baud_rate);
    let integer = divisor64 / 64;
    if integer == 0 || integer > u64::from(u16::MAX) {
        return Err(Error::BaudRateOutOfRange);
    }
    Ok((integer as u32, (divisor64 % 64) as u32))
}

impl Console for Pl011 {
    fn try_write_byte(&self, byte: u8) -> bool {
        self.try_write(byte)
    }
}

impl DriverInstance for Pl011 {
    fn activate(&mut self) -> Result<(), ProbeError> {
        // Generic platform binding does not own a serial-input consumer or IRQ
        // registration. Remaining quiescent is therefore its active state.
        self.disable_interrupts();
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), ProbeError> {
        self.disable_interrupts();
        self.wait_until_idle();
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ProbeError> {
        self.disable_interrupts();
        Ok(())
    }

    fn remove(&mut self) -> Result<(), ProbeError> {
        self.disable_interrupts();
        Ok(())
    }
}

pub struct Pl011PlatformDriver;

impl PlatformDriver for Pl011PlatformDriver {
    fn name(&self) -> &'static str {
        "pl011"
    }

    fn compatible_table(&self) -> &'static [&'static str] {
        &["arm,pl011"]
    }

    fn probe(
        &self,
        device: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError> {
        let registers = device.registers().first().ok_or(ProbeError::Resource)?;
        let mapping = services
            .map_mmio(*registers)
            .map_err(|_| ProbeError::Resource)?;
        let uart = Pl011::from_mapped_mmio(mapping).map_err(|_| ProbeError::Resource)?;
        // Firmware may have left receive/error sources enabled. Probe must
        // return a quiescent instance because manager ownership is not yet
        // published and no handler context exists for this generic binding.
        uart.disable_interrupts();
        let instance: Box<dyn DriverInstance> =
            hyper::mm::try_box(uart).map_err(|_| ProbeError::Resource)?;
        Ok(instance)
    }
}

pub static PLATFORM_DRIVER: Pl011PlatformDriver = Pl011PlatformDriver;
