// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Register models for the initial x86 legacy PC virtual board.

use crate::device::uart16550::Ns16550;
const COM1_BASE: u16 = 0x3f8;
const MASTER_PIC_COMMAND: u16 = 0x20;
const MASTER_PIC_DATA: u16 = 0x21;
const SLAVE_PIC_COMMAND: u16 = 0xa0;
const SLAVE_PIC_DATA: u16 = 0xa1;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAccessSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortAccess {
    pub value: Option<u32>,
    pub transmitted: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptSource {
    Timer,
    Com1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingInterrupt {
    pub vector: u8,
    pub source: InterruptSource,
}

impl PortAccess {
    const fn read(value: u32) -> Self {
        Self {
            value: Some(value),
            transmitted: None,
        }
    }

    const fn write(transmitted: Option<u8>) -> Self {
        Self {
            value: None,
            transmitted,
        }
    }
}

/// Minimal legacy-PC device set needed by the first x86 Linux guest.
pub struct LegacyPcDevices {
    master_pic: I8259,
    slave_pic: I8259,
    pit: Pit,
    com1: Ns16550,
}

impl LegacyPcDevices {
    pub const fn new() -> Self {
        Self {
            master_pic: I8259::new(0x08),
            slave_pic: I8259::new(0x70),
            pit: Pit::new(),
            com1: Ns16550::new(),
        }
    }

    pub fn access(
        &mut self,
        port: u16,
        size: usize,
        write: bool,
        value: u32,
    ) -> Result<PortAccess, Error> {
        if !matches!(size, 1 | 2 | 4) {
            return Err(Error::InvalidAccessSize);
        }
        if (COM1_BASE..COM1_BASE + 8).contains(&port) && size == 1 {
            return Ok(if write {
                PortAccess::write(self.com1.write((port - COM1_BASE) as usize, value as u8))
            } else {
                PortAccess::read(u32::from(self.com1.read((port - COM1_BASE) as usize)))
            });
        }
        if size == 1 {
            match (port, write) {
                (MASTER_PIC_COMMAND, true) => self.master_pic.write_command(value as u8),
                (MASTER_PIC_DATA, true) => self.master_pic.write_data(value as u8),
                (SLAVE_PIC_COMMAND, true) => self.slave_pic.write_command(value as u8),
                (SLAVE_PIC_DATA, true) => self.slave_pic.write_data(value as u8),
                (PIT_CHANNEL0, true) => self.pit.write_channel0(value as u8),
                (PIT_COMMAND, true) => self.pit.write_command(value as u8),
                (MASTER_PIC_COMMAND, false) => {
                    return Ok(PortAccess::read(u32::from(self.master_pic.read_command())));
                }
                (MASTER_PIC_DATA, false) => {
                    return Ok(PortAccess::read(u32::from(self.master_pic.mask())));
                }
                (SLAVE_PIC_COMMAND, false) => {
                    return Ok(PortAccess::read(u32::from(self.slave_pic.read_command())));
                }
                (SLAVE_PIC_DATA, false) => {
                    return Ok(PortAccess::read(u32::from(self.slave_pic.mask())));
                }
                (PIT_CHANNEL0 | PIT_COMMAND, false) => return Ok(PortAccess::read(0)),
                _ => return Ok(default_access(size, write)),
            }
            return Ok(PortAccess::write(None));
        }
        Ok(default_access(size, write))
    }

    pub fn timer_vector(&self) -> Option<u8> {
        (self.master_pic.mask() & 1 == 0).then_some(self.master_pic.vector_offset())
    }

    pub fn pending_interrupt(&self, timer_pending: bool) -> Option<PendingInterrupt> {
        if timer_pending && self.master_pic.mask() & 1 == 0 {
            return Some(PendingInterrupt {
                vector: self.master_pic.vector_offset(),
                source: InterruptSource::Timer,
            });
        }
        if self.master_pic.mask() & (1 << 4) == 0 && self.com1.interrupt_asserted() {
            return Some(PendingInterrupt {
                vector: self.master_pic.vector_offset() + 4,
                source: InterruptSource::Com1,
            });
        }
        None
    }
}

impl Default for LegacyPcDevices {
    fn default() -> Self {
        Self::new()
    }
}

const fn default_access(size: usize, write: bool) -> PortAccess {
    if write {
        PortAccess::write(None)
    } else {
        PortAccess::read(match size {
            1 => 0xff,
            2 => 0xffff,
            _ => u32::MAX,
        })
    }
}

struct I8259 {
    vector_offset: u8,
    mask: u8,
    initialization_step: u8,
    read_isr: bool,
}

impl I8259 {
    const fn new(vector_offset: u8) -> Self {
        Self {
            vector_offset,
            mask: u8::MAX,
            initialization_step: 0,
            read_isr: false,
        }
    }

    fn write_command(&mut self, value: u8) {
        if value & 0x10 != 0 {
            self.initialization_step = 1;
            self.mask = u8::MAX;
        } else if value & 0x18 == 0x08 {
            self.read_isr = value & 1 != 0;
        }
    }

    fn write_data(&mut self, value: u8) {
        match self.initialization_step {
            1 => {
                self.vector_offset = value & 0xf8;
                self.initialization_step = 2;
            }
            2 => self.initialization_step = 3,
            3 => self.initialization_step = 0,
            _ => self.mask = value,
        }
    }

    const fn read_command(&self) -> u8 {
        let _ = self.read_isr;
        0
    }

    const fn vector_offset(&self) -> u8 {
        self.vector_offset
    }

    const fn mask(&self) -> u8 {
        self.mask
    }
}

struct Pit {
    command: u8,
    channel0: [u8; 2],
    channel0_index: usize,
}

impl Pit {
    const fn new() -> Self {
        Self {
            command: 0,
            channel0: [0; 2],
            channel0_index: 0,
        }
    }

    fn write_command(&mut self, value: u8) {
        self.command = value;
        self.channel0_index = 0;
    }

    fn write_channel0(&mut self, value: u8) {
        self.channel0[self.channel0_index] = value;
        self.channel0_index ^= 1;
    }
}
