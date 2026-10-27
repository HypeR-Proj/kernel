// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RV64 integer MMIO decoding. No guest memory is accessed by this module.

use {crate::exit::AccessWidth, crate::exit::MemoryAccess};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestInstruction {
    Fetched(u32),
    Transformed(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Htinst {
    Unavailable,
    Instruction(GuestInstruction),
    ImplicitPageWalk,
    Unsupported,
}

/// Classifies RV64 HTINST without confusing an implicit PTE access with MMIO.
/// Unknown values may be handled by fetching the original instruction instead.
pub const fn classify_htinst(value: u64) -> Htinst {
    match value {
        0 => Htinst::Unavailable,
        0x3000 | 0x3020 => Htinst::ImplicitPageWalk,
        value if value <= u32::MAX as u64 && value & 1 != 0 => {
            Htinst::Instruction(GuestInstruction::Transformed(value as u32))
        }
        _ => Htinst::Unsupported,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioRegister {
    Load { rd: u8, signed: bool },
    Store { rs2: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveAddress {
    pub base: u8,
    pub displacement: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioInstruction {
    /// Original effective address, when the instruction was fetched. Comparing
    /// this with STVAL rejects faults on a later part of a misaligned access.
    pub address: Option<EffectiveAddress>,
    pub width: AccessWidth,
    pub operation: MmioRegister,
    pub instruction_bytes: u8,
    /// Transformed accesses can fault after part of a misaligned access. Device
    /// emulation must reject a nonzero offset instead of replaying the access.
    pub address_offset: u8,
}

impl MmioInstruction {
    pub const fn read_value(self, value: u64) -> u64 {
        let bits = self.width.bytes() * 8;
        let shift = 64 - bits;
        match self.operation {
            MmioRegister::Load { signed: true, .. } => ((value << shift) as i64 >> shift) as u64,
            _ => (value << shift) >> shift,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnsupportedInstruction,
    AccessMismatch,
}

pub fn decode_mmio(
    instruction: GuestInstruction,
    access: MemoryAccess,
) -> Result<MmioInstruction, DecodeError> {
    let (word, length, address_offset) = match instruction {
        GuestInstruction::Fetched(word) if word & 3 != 3 => {
            return decode_compressed(word as u16, access);
        }
        GuestInstruction::Fetched(word) => (word, 4, 0),
        GuestInstruction::Transformed(word) if word & 1 != 0 => (
            word | 2,
            if word & 2 == 0 { 2 } else { 4 },
            ((word >> 15) & 31) as u8,
        ),
        _ => return Err(DecodeError::UnsupportedInstruction),
    };
    let function = (word >> 12) & 7;
    let (width, operation) = match word & 0x7f {
        0x03 if function <= 6 => (
            width(function & 3)?,
            MmioRegister::Load {
                rd: ((word >> 7) & 31) as u8,
                signed: function < 4,
            },
        ),
        0x23 if function <= 3 => (
            width(function)?,
            MmioRegister::Store {
                rs2: ((word >> 20) & 31) as u8,
            },
        ),
        _ => return Err(DecodeError::UnsupportedInstruction),
    };
    let address = match instruction {
        GuestInstruction::Fetched(_) => {
            let immediate = if word & 0x7f == 0x03 {
                word >> 20
            } else {
                ((word >> 25) << 5) | ((word >> 7) & 31)
            };
            Some(EffectiveAddress {
                base: ((word >> 15) & 31) as u8,
                displacement: ((immediate as i16) << 4) >> 4,
            })
        }
        GuestInstruction::Transformed(_) => None,
    };
    finish(width, operation, length, address_offset, address, access)
}

fn width(function: u32) -> Result<AccessWidth, DecodeError> {
    match function {
        0 => Ok(AccessWidth::Byte),
        1 => Ok(AccessWidth::HalfWord),
        2 => Ok(AccessWidth::Word),
        3 => Ok(AccessWidth::DoubleWord),
        _ => Err(DecodeError::UnsupportedInstruction),
    }
}

fn decode_compressed(word: u16, access: MemoryAccess) -> Result<MmioInstruction, DecodeError> {
    let function = word >> 13;
    let width = match function {
        2 | 6 => AccessWidth::Word,
        3 | 7 => AccessWidth::DoubleWord,
        _ => return Err(DecodeError::UnsupportedInstruction),
    };
    let operation = match (word & 3, function) {
        (0, 2 | 3) => MmioRegister::Load {
            rd: ((word >> 2) & 7) as u8 + 8,
            signed: true,
        },
        (0, 6 | 7) => MmioRegister::Store {
            rs2: ((word >> 2) & 7) as u8 + 8,
        },
        (2, 2 | 3) if (word >> 7) & 31 != 0 => MmioRegister::Load {
            rd: ((word >> 7) & 31) as u8,
            signed: true,
        },
        (2, 6 | 7) => MmioRegister::Store {
            rs2: ((word >> 2) & 31) as u8,
        },
        _ => return Err(DecodeError::UnsupportedInstruction),
    };
    let base = if word & 3 == 0 {
        ((word >> 7) & 7) as u8 + 8
    } else {
        2
    };
    let displacement = match (word & 3, function) {
        (0, 2 | 6) => ((word >> 10) & 7) << 3 | ((word >> 6) & 1) << 2 | ((word >> 5) & 1) << 6,
        (0, 3 | 7) => ((word >> 10) & 7) << 3 | ((word >> 5) & 3) << 6,
        (2, 2) => ((word >> 12) & 1) << 5 | ((word >> 4) & 7) << 2 | ((word >> 2) & 3) << 6,
        (2, 3) => ((word >> 12) & 1) << 5 | ((word >> 5) & 3) << 3 | ((word >> 2) & 7) << 6,
        (2, 6) => ((word >> 9) & 15) << 2 | ((word >> 7) & 3) << 6,
        (2, 7) => ((word >> 10) & 7) << 3 | ((word >> 7) & 7) << 6,
        _ => return Err(DecodeError::UnsupportedInstruction),
    };
    finish(
        width,
        operation,
        2,
        0,
        Some(EffectiveAddress {
            base,
            displacement: displacement as i16,
        }),
        access,
    )
}

fn finish(
    width: AccessWidth,
    operation: MmioRegister,
    instruction_bytes: u8,
    address_offset: u8,
    address: Option<EffectiveAddress>,
    access: MemoryAccess,
) -> Result<MmioInstruction, DecodeError> {
    if !matches!(
        (operation, access),
        (MmioRegister::Load { .. }, MemoryAccess::Read)
            | (MmioRegister::Store { .. }, MemoryAccess::Write)
    ) {
        return Err(DecodeError::AccessMismatch);
    }
    Ok(MmioInstruction {
        address,
        width,
        operation,
        instruction_bytes,
        address_offset,
    })
}
