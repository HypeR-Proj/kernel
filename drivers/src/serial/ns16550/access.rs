// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Register-addressing backend for physical NS16550-compatible UARTs.
//!
//! This module is the only layer that turns integer MMIO addresses or port-I/O
//! capabilities into hardware accesses. `RegisterBank` construction validates
//! the complete eight-register window once; the UART model above it can then
//! use the closed `Register` vocabulary without pointer arithmetic or unsafe
//! operations. Handles are shareable mapping capabilities, not exclusive
//! hardware ownership, so configuration remains serialized by the subsystem
//! that binds the UART.

use core::ptr::{read_volatile, write_volatile};

use crate::platform::PermanentMmioMapping;
use hyper::hal::io::PortIo;

use super::Error;

/// Width and spacing of the eight standard UART registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioAccess {
    Byte { register_shift: u8 },
    Word { register_shift: u8 },
}

impl MmioAccess {
    pub const BYTE: Self = Self::Byte { register_shift: 0 };
    pub const WORD: Self = Self::Word { register_shift: 2 };

    pub const fn register_shift(self) -> u8 {
        match self {
            Self::Byte { register_shift } | Self::Word { register_shift } => register_shift,
        }
    }

    pub(super) const fn register_window(self) -> Option<u64> {
        let Some(stride) = 1u64.checked_shl(self.register_shift() as u32) else {
            return None;
        };
        let width = match self {
            Self::Byte { .. } => 1,
            Self::Word { .. } => core::mem::size_of::<u32>() as u64,
        };
        let Some(last_offset) = 7u64.checked_mul(stride) else {
            return None;
        };
        last_offset.checked_add(width)
    }

    const fn valid(self, base: usize) -> bool {
        let shift = self.register_shift();
        shift < usize::BITS as u8
            && match self {
                Self::Byte { .. } => true,
                Self::Word { .. } => base.is_multiple_of(4) && shift >= 2,
            }
    }
}

/// One of the eight registers present in every NS16550-compatible bank.
#[derive(Clone, Copy)]
pub(super) struct Register(u8);

impl Register {
    pub(super) const DATA: Self = Self(0);
    pub(super) const INTERRUPT_ENABLE: Self = Self(1);
    pub(super) const INTERRUPT_IDENTIFICATION: Self = Self(2);
    pub(super) const LINE_CONTROL: Self = Self(3);
    pub(super) const MODEM_CONTROL: Self = Self(4);
    pub(super) const LINE_STATUS: Self = Self(5);
    pub(super) const MODEM_STATUS: Self = Self(6);
    pub(super) const SCRATCH: Self = Self(7);

    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Validated access capability for one complete physical UART register bank.
#[derive(Clone, Copy)]
pub(super) struct RegisterBank {
    base: usize,
    access: RegisterAccess,
}

#[derive(Clone, Copy)]
enum RegisterAccess {
    Mmio(MmioAccess),
    Port(PortIo),
}

impl RegisterBank {
    pub(super) fn from_mapped_mmio(
        mapping: PermanentMmioMapping,
        access: MmioAccess,
    ) -> Result<Self, Error> {
        let window = access.register_window().ok_or(Error::AddressOverflow)?;
        let alignment = match access {
            MmioAccess::Byte { .. } => 1,
            MmioAccess::Word { .. } => core::mem::align_of::<u32>(),
        };
        let mapping = mapping
            .validate_window(window, alignment)
            .map_err(|error| match error {
                crate::platform::MmioMappingError::AddressOverflow => Error::AddressOverflow,
                crate::platform::MmioMappingError::NotMapped
                | crate::platform::MmioMappingError::WindowTooSmall
                | crate::platform::MmioMappingError::Misaligned
                | crate::platform::MmioMappingError::InvalidAlignment => Error::InvalidAccess,
            })?;
        // SAFETY: The mapping capability covers the validated register window
        // permanently. This module retains all subsequent pointer arithmetic.
        unsafe { Self::from_mmio(mapping.virtual_start(), access) }
    }

    /// # Safety
    ///
    /// `base` must identify the complete register window with device memory
    /// attributes for every copied bank. Hardware reconfiguration must be
    /// externally serialized.
    pub(super) const unsafe fn from_mmio(base: usize, access: MmioAccess) -> Result<Self, Error> {
        if !access.valid(base) {
            return Err(Error::InvalidAccess);
        }
        let Some(stride) = 1usize.checked_shl(access.register_shift() as u32) else {
            return Err(Error::AddressOverflow);
        };
        let Some(last_offset) = 7usize.checked_mul(stride) else {
            return Err(Error::AddressOverflow);
        };
        let width = match access {
            MmioAccess::Byte { .. } => 1,
            MmioAccess::Word { .. } => core::mem::size_of::<u32>(),
        };
        let Some(window) = last_offset.checked_add(width) else {
            return Err(Error::AddressOverflow);
        };
        if base.checked_add(window).is_none() {
            return Err(Error::AddressOverflow);
        }
        Ok(Self {
            base,
            access: RegisterAccess::Mmio(access),
        })
    }

    /// # Safety
    ///
    /// The caller must own all eight consecutive ports beginning at `base`.
    pub(super) const unsafe fn from_port(base: u16, io: PortIo) -> Result<Self, Error> {
        if base > u16::MAX - 7 {
            return Err(Error::AddressOverflow);
        }
        Ok(Self {
            base: base as usize,
            access: RegisterAccess::Port(io),
        })
    }

    pub(super) const fn mmio_base(self) -> usize {
        self.base
    }

    pub(super) const fn mmio_access(self) -> Option<MmioAccess> {
        match self.access {
            RegisterAccess::Mmio(access) => Some(access),
            RegisterAccess::Port(_) => None,
        }
    }

    pub(super) fn read(self, register: Register) -> u8 {
        let index = register.index();
        // SAFETY: Constructors validate the entire eight-register bank. The
        // closed Register type proves `index <= 7`, so address arithmetic stays
        // within that bank and preserves the selected access width.
        unsafe {
            match self.access {
                RegisterAccess::Mmio(MmioAccess::Byte { register_shift }) => {
                    read_volatile((self.base + (index << register_shift)) as *const u8)
                }
                RegisterAccess::Mmio(MmioAccess::Word { register_shift }) => {
                    read_volatile((self.base + (index << register_shift)) as *const u32) as u8
                }
                RegisterAccess::Port(io) => io.read((self.base + index) as u16),
            }
        }
    }

    pub(super) fn write(self, register: Register, value: u8) {
        let index = register.index();
        // SAFETY: The same validated-bank and closed-register proof as read
        // applies. MMIO uses volatile operations and port access delegates to
        // the architecture-issued PortIo capability.
        unsafe {
            match self.access {
                RegisterAccess::Mmio(MmioAccess::Byte { register_shift }) => {
                    write_volatile((self.base + (index << register_shift)) as *mut u8, value)
                }
                RegisterAccess::Mmio(MmioAccess::Word { register_shift }) => write_volatile(
                    (self.base + (index << register_shift)) as *mut u32,
                    u32::from(value),
                ),
                RegisterAccess::Port(io) => io.write((self.base + index) as u16, value),
            }
        }
    }
}
