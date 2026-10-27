// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure Intel VMX capability and exit decoding.

pub const VMX_REGION_MEMORY_TYPE_WB: u8 = 6;
pub const VMX_REGION_MAX_SIZE: u16 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmxBasic {
    pub revision: u32,
    pub region_size: u16,
    pub memory_type: u8,
    pub physical_address_width: VmxPhysicalAddressWidth,
    pub true_controls: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmxPhysicalAddressWidth {
    Bits32,
    Processor,
}

impl VmxBasic {
    pub const fn decode(value: u64) -> Self {
        Self {
            revision: value as u32 & 0x7fff_ffff,
            region_size: ((value >> 32) & 0x1fff) as u16,
            physical_address_width: if value & (1 << 48) == 0 {
                VmxPhysicalAddressWidth::Processor
            } else {
                VmxPhysicalAddressWidth::Bits32
            },
            memory_type: ((value >> 50) & 0xf) as u8,
            true_controls: value & (1 << 55) != 0,
        }
    }

    pub const fn is_supported(self) -> bool {
        self.region_size != 0
            && self.region_size <= VMX_REGION_MAX_SIZE
            && self.memory_type == VMX_REGION_MEMORY_TYPE_WB
    }

    pub const fn accepts_region(self, physical_address: u64) -> bool {
        physical_address.is_multiple_of(VMX_REGION_MAX_SIZE as u64)
            && match self.physical_address_width {
                VmxPhysicalAddressWidth::Bits32 => physical_address <= u32::MAX as u64,
                VmxPhysicalAddressWidth::Processor => true,
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlCapability {
    pub must_be_one: u32,
    pub may_be_one: u32,
}

impl ControlCapability {
    pub const fn decode(value: u64) -> Self {
        Self {
            must_be_one: value as u32,
            may_be_one: (value >> 32) as u32,
        }
    }

    pub const fn apply(self, requested: u32) -> Option<u32> {
        let value = (requested | self.must_be_one) & self.may_be_one;
        if value & requested == requested {
            Some(value)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoExit {
    pub port: u16,
    pub size: usize,
    pub direction: IoDirection,
    pub string: bool,
    pub repeat: bool,
}

impl IoExit {
    pub const fn decode(qualification: u64) -> Option<Self> {
        let size = match qualification & 7 {
            0 => 1,
            1 => 2,
            3 => 4,
            _ => return None,
        };
        Some(Self {
            port: (qualification >> 16) as u16,
            size,
            direction: if qualification & (1 << 3) == 0 {
                IoDirection::Output
            } else {
                IoDirection::Input
            },
            string: qualification & (1 << 4) != 0,
            repeat: qualification & (1 << 5) != 0,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EptAccess {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EptViolation {
    pub access: EptAccess,
    pub during_page_walk: bool,
}

impl EptViolation {
    pub const fn decode(qualification: u64) -> Self {
        let access = if qualification & (1 << 1) != 0 {
            EptAccess::Write
        } else if qualification & (1 << 2) != 0 {
            EptAccess::Execute
        } else {
            EptAccess::Read
        };
        Self {
            access,
            // Bit 8 marks a final guest-physical translation. Its absence
            // means the access came from the guest page walk itself.
            during_page_walk: qualification & (1 << 8) == 0,
        }
    }
}
