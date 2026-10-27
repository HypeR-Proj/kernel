// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Early-console discovery and safe driver binding.

use hyper::hal::console::Console;
use hyper::hal::io::PortIo;
use {
    hyper_platform::ConsoleInfo, hyper_platform::ConsoleKind,
    hyper_platform::ConsoleRegisterAccess, hyper_platform::chosen::CommandLine,
};

use super::{MmioAccess, Ns16550, Ns16550Error, Pl011};
use crate::platform::{MmioMappingError, PermanentMmioMapping};

/// A platform-selected early console driver.
#[derive(Clone, Copy)]
pub enum ConsoleDevice {
    /// An ARM `PrimeCell` PL011 UART.
    Pl011(Pl011),
    /// An NS16550-compatible UART.
    Ns16550(Ns16550),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Errors returned while parsing an `earlycon` command-line option.
pub enum EarlyConsoleError {
    /// The supplied register address was absent, malformed, or unaligned.
    InvalidAddress,
    /// The selected console driver did not include a register address.
    MissingAddress,
    /// The selected console driver is not supported.
    UnsupportedDriver,
}

/// Failure to bind a selected console to its hardware capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindError {
    /// An MMIO console was not given a permanent mapping capability.
    MissingMapping,
    /// The mapping does not satisfy the selected controller's register layout.
    InvalidMapping(MmioMappingError),
    /// The NS16550 register or port layout is invalid.
    Ns16550(Ns16550Error),
}

impl From<Ns16550Error> for BindError {
    fn from(error: Ns16550Error) -> Self {
        Self::Ns16550(error)
    }
}

#[derive(Clone, Copy)]
/// Encoded console state that remains usable in fatal paths.
pub struct EmergencyConsoleHandle {
    /// The mapped UART base address.
    pub base: usize,
    /// Driver-specific encoding of the access mode.
    pub metadata: usize,
}

/// Parses the explicit-address subset of Linux's `earlycon=` command line.
///
/// Supported drivers are PL011 and NS16550/8250. NS16550 accepts Linux's
/// byte-wide `mmio` and word-wide, word-spaced `mmio32` selectors. Firmware is
/// expected to have configured the UART before entry, so optional
/// serial-format fields are not consumed.
pub fn early_console(
    command_line: Option<&CommandLine>,
) -> Result<Option<ConsoleInfo>, EarlyConsoleError> {
    let Some(value) = command_line.and_then(|arguments| arguments.value("earlycon")) else {
        return Ok(None);
    };
    if value == "off" {
        return Ok(None);
    }
    let mut fields = value.split(',');
    let driver = fields.next().ok_or(EarlyConsoleError::UnsupportedDriver)?;
    let mut address = fields.next().ok_or(EarlyConsoleError::MissingAddress)?;
    let access = matches!(address, "mmio" | "mmio32" | "io").then_some(address);
    if access.is_some() {
        address = fields.next().ok_or(EarlyConsoleError::MissingAddress)?;
    }
    let address = parse_address(address).ok_or(EarlyConsoleError::InvalidAddress)?;
    if address == 0 {
        return Err(EarlyConsoleError::InvalidAddress);
    }
    let (kind, access) = match driver {
        "pl011" => (ConsoleKind::Pl011, ConsoleRegisterAccess::Native),
        "uart8250" | "ns16550" => (
            ConsoleKind::Ns16550,
            if access == Some("io") {
                ConsoleRegisterAccess::Port
            } else if access == Some("mmio32") {
                ConsoleRegisterAccess::Mmio32 { register_shift: 2 }
            } else {
                ConsoleRegisterAccess::Mmio8 { register_shift: 0 }
            },
        ),
        _ => return Err(EarlyConsoleError::UnsupportedDriver),
    };
    let required_alignment = match access {
        ConsoleRegisterAccess::Native | ConsoleRegisterAccess::Mmio32 { .. } => 4,
        ConsoleRegisterAccess::Port | ConsoleRegisterAccess::Mmio8 { .. } => 1,
    };
    if !address.is_multiple_of(required_alignment) {
        return Err(EarlyConsoleError::InvalidAddress);
    }
    Ok(Some(ConsoleInfo {
        kind,
        base: address,
        access,
    }))
}

fn parse_address(value: &str) -> Option<u64> {
    let (digits, radix) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(digits) => (digits, 16),
        None => (value, 10),
    };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

impl Console for ConsoleDevice {
    fn try_write_byte(&self, byte: u8) -> bool {
        match self {
            Self::Pl011(device) => device.try_write(byte),
            Self::Ns16550(device) => device.try_write(byte),
        }
    }
}

impl ConsoleDevice {
    /// Attempts to read one byte without blocking.
    #[cfg(feature = "crash-console")]
    pub fn try_read_byte(&self) -> Option<u8> {
        match self {
            Self::Pl011(device) => device.try_read().map(|received| received.byte),
            Self::Ns16550(device) => device.try_read().map(|received| received.byte),
        }
    }

    /// Encodes a lock-free handle retained by fatal console paths.
    pub const fn emergency_handle(self) -> EmergencyConsoleHandle {
        match self {
            Self::Pl011(device) => EmergencyConsoleHandle {
                base: device.mmio_base(),
                metadata: 1,
            },
            Self::Ns16550(device) => {
                let (kind, shift) = match device.mmio_access() {
                    Some(MmioAccess::Byte { register_shift }) => (2, register_shift),
                    Some(MmioAccess::Word { register_shift }) => (6, register_shift),
                    None => (3, 0),
                };
                EmergencyConsoleHandle {
                    base: device.mmio_base(),
                    metadata: kind | ((shift as usize) << 3),
                }
            }
        }
    }

    /// Reconstructs a console from a handle returned by [`Self::emergency_handle`].
    ///
    /// # Safety
    ///
    /// The encoded MMIO mapping must still be valid and exclusively owned by
    /// the console subsystem.
    pub unsafe fn from_emergency_handle(
        handle: EmergencyConsoleHandle,
        port_io: Option<PortIo>,
    ) -> Option<Self> {
        match handle.metadata & 3 {
            // SAFETY: This unsafe constructor inherits the handle mapping and
            // ownership contract documented by this function.
            1 => Some(Self::Pl011(unsafe { Pl011::from_mmio_base(handle.base) })),
            2 => {
                let shift = (handle.metadata >> 3) as u8;
                let access = if handle.metadata & (1 << 2) == 0 {
                    MmioAccess::Byte {
                        register_shift: shift,
                    }
                } else {
                    MmioAccess::Word {
                        register_shift: shift,
                    }
                };
                // SAFETY: The encoded handle retains the live owned mapping;
                // from_mmio validates the reconstructed register layout.
                match unsafe { Ns16550::from_mmio(handle.base, access) } {
                    Ok(device) => Some(Self::Ns16550(device)),
                    Err(_) => None,
                }
            }
            3 => match (u16::try_from(handle.base), port_io) {
                // SAFETY: The handle identifies the previously owned port bank
                // and `io` is the architecture capability used to create it.
                (Ok(base), Some(io)) => match unsafe { Ns16550::from_port(base, io) } {
                    Ok(device) => Some(Self::Ns16550(device)),
                    Err(_) => None,
                },
                _ => None,
            },
            _ => None,
        }
    }
}

/// Binds a discovered console description to a permanent register mapping.
///
/// Port-I/O consoles require `port_io`; MMIO consoles require `mapping`.
pub fn bind(
    info: ConsoleInfo,
    mapping: Option<PermanentMmioMapping>,
    port_io: Option<PortIo>,
) -> Result<ConsoleDevice, BindError> {
    match info.kind {
        ConsoleKind::Pl011 => {
            let mapping = mapping.ok_or(BindError::MissingMapping)?;
            Pl011::from_mapped_mmio(mapping)
                .map(ConsoleDevice::Pl011)
                .map_err(BindError::InvalidMapping)
        }
        ConsoleKind::Ns16550 => {
            if info.access == ConsoleRegisterAccess::Port {
                let port = u16::try_from(info.base)
                    .map_err(|_| BindError::Ns16550(Ns16550Error::InvalidAccess))?;
                let io = port_io.ok_or(BindError::Ns16550(Ns16550Error::InvalidAccess))?;
                // SAFETY: The platform console selection owns the complete
                // eight-port bank and keeps `io` valid for the handle lifetime.
                return unsafe { Ns16550::from_port(port, io) }
                    .map(ConsoleDevice::Ns16550)
                    .map_err(BindError::Ns16550);
            }
            let access = console_mmio_access(info.access)?;
            let mapping = mapping.ok_or(BindError::MissingMapping)?;
            Ns16550::from_mapped_mmio(mapping, access)
                .map(ConsoleDevice::Ns16550)
                .map_err(BindError::Ns16550)
        }
    }
}

/// Binds the early console while the architecture bootstrap mapping is active.
///
/// # Safety
///
/// `mapped_base` must map the register range described by `info` with Device
/// memory attributes until the kernel promotes the console to its permanent
/// mapping. The console subsystem must be the only configuration owner.
pub unsafe fn bind_bootstrap(
    info: ConsoleInfo,
    mapped_base: usize,
    port_io: Option<PortIo>,
) -> Result<ConsoleDevice, BindError> {
    match info.kind {
        ConsoleKind::Pl011 => {
            // SAFETY: The caller transfers the validated MMIO range to the
            // selected driver implementation.
            Ok(ConsoleDevice::Pl011(unsafe {
                Pl011::from_mmio_base(mapped_base)
            }))
        }
        ConsoleKind::Ns16550 => {
            if info.access == ConsoleRegisterAccess::Port {
                let port = u16::try_from(info.base)
                    .map_err(|_| BindError::Ns16550(Ns16550Error::InvalidAccess))?;
                let io = port_io.ok_or(BindError::Ns16550(Ns16550Error::InvalidAccess))?;
                // SAFETY: The caller owns the complete port resource and the
                // conversion above proves all eight ports are representable.
                return unsafe { Ns16550::from_port(port, io) }
                    .map(ConsoleDevice::Ns16550)
                    .map_err(BindError::Ns16550);
            }
            let access = console_mmio_access(info.access)?;
            // SAFETY: The caller transfers the validated MMIO mapping.
            unsafe { Ns16550::from_mmio(mapped_base, access) }
                .map(ConsoleDevice::Ns16550)
                .map_err(BindError::Ns16550)
        }
    }
}

fn console_mmio_access(access: ConsoleRegisterAccess) -> Result<MmioAccess, Ns16550Error> {
    match access {
        ConsoleRegisterAccess::Mmio8 { register_shift } => Ok(MmioAccess::Byte { register_shift }),
        ConsoleRegisterAccess::Mmio32 { register_shift } => Ok(MmioAccess::Word { register_shift }),
        ConsoleRegisterAccess::Native => Ok(MmioAccess::BYTE),
        ConsoleRegisterAccess::Port => Err(Ns16550Error::InvalidAccess),
    }
}
