// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent contracts for the x86 virtual CPU model.

pub mod device;
pub mod exit;
pub mod svm;
pub mod vmx;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl CpuidResult {
    pub const ZERO: Self = Self {
        eax: 0,
        ebx: 0,
        ecx: 0,
        edx: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GuestMsr {
    TimestampCounter = 0x10,
    ApicBase = 0x1b,
    SysenterCs = 0x174,
    SysenterEsp = 0x175,
    SysenterEip = 0x176,
    Pat = 0x277,
    Efer = 0xc000_0080,
    Star = 0xc000_0081,
    Lstar = 0xc000_0082,
    Cstar = 0xc000_0083,
    Sfmask = 0xc000_0084,
    FsBase = 0xc000_0100,
    GsBase = 0xc000_0101,
    KernelGsBase = 0xc000_0102,
    TscAux = 0xc000_0103,
}

impl GuestMsr {
    pub const fn decode(index: u32) -> Option<Self> {
        Some(match index {
            0x10 => Self::TimestampCounter,
            0x1b => Self::ApicBase,
            0x174 => Self::SysenterCs,
            0x175 => Self::SysenterEsp,
            0x176 => Self::SysenterEip,
            0x277 => Self::Pat,
            0xc000_0080 => Self::Efer,
            0xc000_0081 => Self::Star,
            0xc000_0082 => Self::Lstar,
            0xc000_0083 => Self::Cstar,
            0xc000_0084 => Self::Sfmask,
            0xc000_0100 => Self::FsBase,
            0xc000_0101 => Self::GsBase,
            0xc000_0102 => Self::KernelGsBase,
            0xc000_0103 => Self::TscAux,
            _ => return None,
        })
    }

    pub const fn index(self) -> u32 {
        self as u32
    }
}

pub const fn hypervisor_cpuid() -> CpuidResult {
    CpuidResult {
        eax: 0x4000_0000,
        ebx: u32::from_le_bytes(*b"Hype"),
        ecx: u32::from_le_bytes(*b"R Hy"),
        edx: u32::from_le_bytes(*b"perV"),
    }
}

/// Applies x86 IN instruction semantics to the accumulator.
///
/// Byte and word inputs preserve the upper bits of RAX, while a double-word
/// input writes EAX and therefore clears the upper half of RAX.
pub fn merge_port_input(accumulator: u64, value: u32, size: usize) -> Option<u64> {
    Some(match size {
        1 => (accumulator & !0xff) | u64::from(value & 0xff),
        2 => (accumulator & !0xffff) | u64::from(value & 0xffff),
        4 => u64::from(value),
        _ => return None,
    })
}

pub fn sanitize_cpuid(leaf: u32, subleaf: u32, mut value: CpuidResult) -> CpuidResult {
    match leaf {
        0 => virtualize_vendor(&mut value),
        1 => sanitize_basic_features(&mut value),
        4 => value.eax &= !(0x3f << 26),
        0x0b | 0x1f => value = CpuidResult::ZERO,
        7 if subleaf == 0 => sanitize_structured_features(&mut value),
        7 => value = CpuidResult::ZERO,
        0x0d | 0x0f | 0x10 | 0x12 | 0x14 => value = CpuidResult::ZERO,
        0x8000_0000 => value.eax = value.eax.min(0x8000_0008),
        0x8000_0001 => {
            value.ecx &= !(1 << 2);
            value.edx &= !(1 << 27);
        }
        0x8000_0008 => {
            value.ebx = 0;
            value.ecx = 0;
            value.edx = 0;
        }
        _ => {}
    }
    value
}

fn virtualize_vendor(value: &mut CpuidResult) {
    // A stable virtual vendor avoids implying support for host-vendor MSRs or
    // fixed chipset MMIO that are outside this virtual CPU contract.
    value.ebx = u32::from_le_bytes(*b"Hype");
    value.edx = u32::from_le_bytes(*b"R CP");
    value.ecx = u32::from_le_bytes(*b"U   ");
}

fn sanitize_basic_features(value: &mut CpuidResult) {
    value.ebx = (value.ebx & !0xffff_0000) | (1 << 16);
    value.ecx &= !((1 << 3)
        | (1 << 5)
        | (1 << 12)
        | (1 << 21)
        | (1 << 24)
        | (1 << 26)
        | (1 << 27)
        | (1 << 28)
        | (1 << 29));
    value.ecx |= 1 << 31;
    value.edx &= !((1 << 7) | (1 << 9) | (1 << 12) | (1 << 14) | (1 << 22) | (1 << 28));
}

fn sanitize_structured_features(value: &mut CpuidResult) {
    value.eax = 0;
    value.ebx &= !((1 << 2)
        | (1 << 5)
        | (1 << 12)
        | (1 << 14)
        | (1 << 15)
        | (1 << 16)
        | (1 << 17)
        | (1 << 21)
        | (1 << 25)
        | (1 << 26)
        | (1 << 27)
        | (1 << 28)
        | (1 << 30)
        | (1 << 31));
    value.ecx = 0;
    value.edx = 0;
}
