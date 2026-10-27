// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Arm IHI 0070 G.b, chapters 4--7. Offsets include the architectural page.

use super::{Environment, Error};
use crate::platform::PermanentMmioMapping;

pub(super) const IDR0: usize = 0;
pub(super) const IDR1: usize = 4;
pub(super) const IDR5: usize = 0x14;
pub(super) const CR0: usize = 0x20;
pub(super) const CR0ACK: usize = 0x24;
pub(super) const CR1: usize = 0x28;
pub(super) const CR2: usize = 0x2c;
pub(super) const GBPA: usize = 0x44;
pub(super) const IRQ_CTRL: usize = 0x50;
pub(super) const IRQ_CTRLACK: usize = 0x54;
pub(super) const GERROR: usize = 0x60;
pub(super) const GERRORN: usize = 0x64;
pub(super) const GERROR_IRQ_CFG0: usize = 0x68;
pub(super) const STRTAB_BASE: usize = 0x80;
pub(super) const STRTAB_BASE_CFG: usize = 0x88;
pub(super) const CMDQ_BASE: usize = 0x90;
pub(super) const CMDQ_PROD: usize = 0x98;
pub(super) const CMDQ_CONS: usize = 0x9c;
pub(super) const EVENTQ_BASE: usize = 0xa0;
pub(super) const EVENTQ_IRQ_CFG0: usize = 0xb0;
pub(super) const EVENTQ_PROD: usize = 0x100a8;
pub(super) const EVENTQ_CONS: usize = 0x100ac;
pub(super) const CMDQEN: u32 = 1 << 3;
pub(super) const EVENTQEN: u32 = 1 << 2;
pub(super) const SMMUEN: u32 = 1;
pub(super) const UPDATE: u32 = 1 << 31;
pub(super) const ABORT: u32 = 1 << 20;
pub(super) const CFGI_ALL: [u64; 2] = [4, 31];
pub(super) const TLBI_NSNH_ALL: [u64; 2] = [0x30, 0];
pub(super) const SYNC: [u64; 2] = [0x46, 0];
pub(super) const TIMEOUT_US: u64 = 1_000_000;

pub(super) struct Registers(PermanentMmioMapping);
impl Registers {
    pub(super) fn new(mapping: PermanentMmioMapping) -> Result<Self, Error> {
        mapping
            .validate_window(0x20000, 8)
            .map_err(|_| Error::Address)?;
        Ok(Self(mapping))
    }
    pub(super) fn identity(&self) -> u64 {
        self.0.resource().start()
    }
    pub(super) fn read<E: Environment>(&self, offset: usize) -> u32 {
        E::synchronize();
        // SAFETY: Internal offsets are aligned 32-bit registers inside the
        // checked permanent Device mapping. The controller owns this interface.
        let value =
            unsafe { core::ptr::read_volatile((self.0.virtual_start() + offset) as *const u32) };
        E::synchronize();
        u32::from_le(value)
    }
    pub(super) fn write<E: Environment>(&self, offset: usize, value: u32) {
        E::synchronize();
        // SAFETY: Internal aligned register in the checked Device window.
        unsafe {
            core::ptr::write_volatile((self.0.virtual_start() + offset) as *mut u32, value.to_le())
        };
        E::synchronize();
    }
    pub(super) fn write64<E: Environment>(&self, offset: usize, value: u64) {
        E::synchronize();
        // SAFETY: Internal aligned 64-bit base register in the checked window.
        unsafe {
            core::ptr::write_volatile((self.0.virtual_start() + offset) as *mut u64, value.to_le())
        };
        E::synchronize();
    }
    pub(super) fn wait<E: Environment>(
        &self,
        offset: usize,
        mask: u32,
        value: u32,
    ) -> Result<(), Error> {
        let start = E::now_microseconds();
        loop {
            let actual = self.read::<E>(offset);
            if actual & mask == value {
                return Ok(());
            }
            if E::now_microseconds().wrapping_sub(start) >= TIMEOUT_US {
                return Err(Error::Timeout {
                    register: offset,
                    value: actual,
                });
            }
            core::hint::spin_loop();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub stream_bits: u8,
    pub address_bits: u8,
    pub(super) physical_size: u8,
    pub(super) command_bits: u8,
    pub(super) event_bits: u8,
    pub(super) vmid_limit: u16,
    pub(super) ats_check: u32,
    pub(super) stream_attributes: u64,
}

impl Capabilities {
    pub fn decode(idr0: u32, idr1: u32, idr5: u32) -> Result<Self, Error> {
        // Stage 2, AArch64 tables, coherent walks, LE, terminate faults, and
        // software-owned queues/tables are required. ATS/PRI remain disabled.
        if idr0 & 0x19 != 0x19
            || matches!((idr0 >> 21) & 3, 1 | 3)
            || (idr0 >> 24) & 3 >= 2
            || (idr0 >> 27) & 3 >= 2
            || idr1 & (3 << 29) != 0
            || idr5 & 7 == 7
            || idr5 & (1 << 4) == 0
            || idr1 & 63 > 32
            || (idr1 >> 21) & 31 > 19
            || (idr1 >> 16) & 31 > 19
        {
            return Err(Error::Unsupported);
        }
        let physical_size = (idr5 as u8 & 7).min(5);
        let address_bits = [32, 36, 40, 42, 44, 48][physical_size as usize];
        // This implementation uses a 39-bit IOVA, L1-rooted 4 KiB walk.
        if address_bits < 40 {
            return Err(Error::Unsupported);
        }
        Ok(Self {
            stream_bits: (idr1 as u8 & 63).min(16),
            address_bits,
            physical_size,
            command_bits: (((idr1 >> 21) & 31) as u8).min(8),
            event_bits: (((idr1 >> 16) & 31) as u8).min(7),
            vmid_limit: if idr0 & (1 << 18) != 0 { u16::MAX } else { 255 },
            ats_check: if idr0 & (1 << 10) != 0 { 1 << 4 } else { 0 },
            stream_attributes: if idr1 & (1 << 27) != 0 { 1 << 44 } else { 0 },
        })
    }
}
