// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure validation and semantic decoding of guest `GICv2` and `GICv3` accesses.

use {crate::exit::AccessWidth, crate::exit::GuestPhysicalAddress};

use super::{DISTRIBUTOR_BASE, DISTRIBUTOR_SIZE, REDISTRIBUTOR_BASE, REDISTRIBUTOR_SIZE};

const FRAME_SIZE: u64 = 0x0001_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Frame {
    Distributor,
    DistributorV2,
    CpuDeactivateV2,
    RedistributorControl,
    RedistributorSgi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitmapRegister {
    Group,
    SetEnable,
    ClearEnable,
    SetPending,
    ClearPending,
    SetActive,
    ClearActive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptRoute {
    interrupt: u32,
}

impl InterruptRoute {
    pub const fn interrupt(self) -> u32 {
        self.interrupt
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRegister {
    CpuDeactivateV2,
    DistributorControl,
    DistributorControlV2,
    DistributorTypeV2,
    PeripheralId2V2,
    DistributorType,
    DistributorType2,
    DistributorImplementer,
    DistributorStatus,
    RedistributorControl,
    RedistributorImplementer,
    RedistributorType,
    RedistributorStatus,
    RedistributorWake,
    PeripheralId2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRegisterDescriptor {
    Bitmap {
        register: BitmapRegister,
        first_interrupt: u32,
    },
    Priority {
        first_interrupt: u32,
        count: u8,
    },
    Configuration {
        first_interrupt: u32,
    },
    Route(InterruptRoute),
    Targets {
        first_interrupt: u32,
        count: u8,
    },
    SoftwareInterrupt,
    SgiPending {
        first_interrupt: u32,
        count: u8,
        set: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelRegister {
    pub(super) kind: ModelRegisterKind,
}

impl ModelRegister {
    /// Returns a non-authoritative description suitable for diagnostics/tests.
    ///
    /// A descriptor cannot be converted back into the opaque validated token.
    pub const fn descriptor(self) -> ModelRegisterDescriptor {
        match self.kind {
            ModelRegisterKind::PrivateEnableV2 { set } => ModelRegisterDescriptor::Bitmap {
                register: if set {
                    BitmapRegister::SetEnable
                } else {
                    BitmapRegister::ClearEnable
                },
                first_interrupt: 0,
            },
            ModelRegisterKind::Bitmap {
                register,
                first_interrupt,
            } => ModelRegisterDescriptor::Bitmap {
                register,
                first_interrupt,
            },
            ModelRegisterKind::Priority {
                first_interrupt,
                count,
                ..
            } => ModelRegisterDescriptor::Priority {
                first_interrupt,
                count,
            },
            ModelRegisterKind::Configuration { first_interrupt } => {
                ModelRegisterDescriptor::Configuration { first_interrupt }
            }
            ModelRegisterKind::Route(route) => ModelRegisterDescriptor::Route(route),
            ModelRegisterKind::Targets {
                first_interrupt,
                count,
            } => ModelRegisterDescriptor::Targets {
                first_interrupt,
                count,
            },
            ModelRegisterKind::SoftwareInterrupt => ModelRegisterDescriptor::SoftwareInterrupt,
            ModelRegisterKind::SgiPending {
                first_interrupt,
                count,
                set,
            } => ModelRegisterDescriptor::SgiPending {
                first_interrupt,
                count,
                set,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ModelRegisterKind {
    PrivateEnableV2 {
        set: bool,
    },
    Bitmap {
        register: BitmapRegister,
        first_interrupt: u32,
    },
    Priority {
        first_interrupt: u32,
        count: u8,
        mask: u8,
    },
    Configuration {
        first_interrupt: u32,
    },
    Route(InterruptRoute),
    Targets {
        first_interrupt: u32,
        count: u8,
    },
    SoftwareInterrupt,
    SgiPending {
        first_interrupt: u32,
        count: u8,
        set: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodedRegister {
    Service(ServiceRegister),
    Model(ModelRegister),
    Reserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    CrossesFrame,
    InvalidRegisterAccess,
}

/// One complete access contained by a single guest GIC frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedAccess {
    frame: Frame,
    register: DecodedRegister,
    redistributor: Option<u32>,
}

impl DecodedAccess {
    pub const fn redistributor(self) -> Option<u32> {
        self.redistributor
    }
    /// Identifies the owning frame for diagnostics and frame-local policy.
    pub const fn frame(self) -> Frame {
        self.frame
    }

    pub const fn register(self) -> DecodedRegister {
        self.register
    }
}

/// Decodes an access whose first byte might belong to the reference `GICv3`.
///
/// `Ok(None)` means the first byte is outside every GIC frame. Once the first
/// byte is owned, overflow, a frame-boundary crossing, or an invalid access to
/// a modeled register is reported instead of being forwarded to another
/// device. Reserved space remains a decoded RAZ/WI register.
pub fn decode_v3(
    address: GuestPhysicalAddress,
    width: AccessWidth,
) -> Result<Option<DecodedAccess>, DecodeError> {
    decode_v3_cpus(address, width, 1)
}

pub fn decode_v3_cpus(
    address: GuestPhysicalAddress,
    width: AccessWidth,
    vcpu_count: u32,
) -> Result<Option<DecodedAccess>, DecodeError> {
    let address = address.get();
    let bytes = width.bytes() as u64;
    let distributor_base = u64::from(DISTRIBUTOR_BASE);
    let distributor_end = distributor_base
        .checked_add(u64::from(DISTRIBUTOR_SIZE))
        .ok_or(DecodeError::CrossesFrame)?;
    if (distributor_base..distributor_end).contains(&address) {
        let end = address
            .checked_add(bytes)
            .ok_or(DecodeError::CrossesFrame)?;
        if end > distributor_end {
            return Err(DecodeError::CrossesFrame);
        }
        return decode_frame(Frame::Distributor, address - distributor_base, width).map(Some);
    }

    let redistributor_base = u64::from(REDISTRIBUTOR_BASE);
    let redistributor_end = redistributor_base
        .checked_add(u64::from(REDISTRIBUTOR_SIZE) * u64::from(vcpu_count))
        .ok_or(DecodeError::CrossesFrame)?;
    if !(redistributor_base..redistributor_end).contains(&address) {
        return Ok(None);
    }
    let end = address
        .checked_add(bytes)
        .ok_or(DecodeError::CrossesFrame)?;
    let cpu = (address - redistributor_base) / (2 * FRAME_SIZE);
    let redistributor_base = redistributor_base + cpu * 2 * FRAME_SIZE;
    let redistributor_end = redistributor_base + 2 * FRAME_SIZE;
    let sgi_base = redistributor_base
        .checked_add(FRAME_SIZE)
        .ok_or(DecodeError::CrossesFrame)?;
    let (frame, base, frame_end) = if address < sgi_base {
        (Frame::RedistributorControl, redistributor_base, sgi_base)
    } else {
        (Frame::RedistributorSgi, sgi_base, redistributor_end)
    };
    if end > frame_end {
        return Err(DecodeError::CrossesFrame);
    }
    decode_frame(frame, address - base, width).map(|mut decoded| {
        decoded.redistributor = Some(cpu as u32);
        Some(decoded)
    })
}

/// Decodes the distributor and trapped DIR page. The first GICV page
/// remains a hardware stage-2 mapping.
pub fn decode_v2(
    address: GuestPhysicalAddress,
    width: AccessWidth,
) -> Result<Option<DecodedAccess>, DecodeError> {
    let address = address.get();
    let dir_base =
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_GICV2_CPU_BASE + 0x1000;
    if (dir_base..dir_base + 0x1000).contains(&address) {
        if address
            .checked_add(width.bytes() as u64)
            .is_none_or(|end| end > dir_base + 0x1000)
        {
            return Err(DecodeError::CrossesFrame);
        }
        return decode_frame(Frame::CpuDeactivateV2, address - dir_base, width).map(Some);
    }
    let base = u64::from(DISTRIBUTOR_BASE);
    if !(base..base + 0x1000).contains(&address) {
        return Ok(None);
    }
    if address
        .checked_add(width.bytes() as u64)
        .is_none_or(|end| end > base + 0x1000)
    {
        return Err(DecodeError::CrossesFrame);
    }
    decode_frame(Frame::DistributorV2, address - base, width).map(Some)
}
fn decode_distributor_v2(offset: u32, width: AccessWidth) -> Result<DecodedRegister, DecodeError> {
    for (base, register) in [
        (0, ServiceRegister::DistributorControlV2),
        (4, ServiceRegister::DistributorTypeV2),
        (8, ServiceRegister::DistributorImplementer),
        (0xfe8, ServiceRegister::PeripheralId2V2),
    ] {
        if let Some(result) = fixed(offset, width, base, AccessWidth::Word, register) {
            return result;
        }
    }
    // No security extensions: Group registers are RAZ/WI, every interrupt is
    // Group 0. GICV_CTLR bit zero therefore controls guest IRQ delivery.
    for (base, register) in [
        (0x100, BitmapRegister::SetEnable),
        (0x180, BitmapRegister::ClearEnable),
        (0x200, BitmapRegister::SetPending),
        (0x280, BitmapRegister::ClearPending),
        (0x300, BitmapRegister::SetActive),
        (0x380, BitmapRegister::ClearActive),
    ] {
        for word in 0..super::INTERRUPT_COUNT / 32 {
            if let Some(result) = bitmap(offset, width, base + word * 4, word * 32, register) {
                return result.map(|decoded| {
                    if word == 0
                        && matches!(
                            register,
                            BitmapRegister::SetEnable | BitmapRegister::ClearEnable
                        )
                    {
                        DecodedRegister::Model(ModelRegister {
                            kind: ModelRegisterKind::PrivateEnableV2 {
                                set: register == BitmapRegister::SetEnable,
                            },
                        })
                    } else {
                        decoded
                    }
                });
            }
        }
    }
    for word in 0..super::INTERRUPT_COUNT / 32 {
        if let Some(result) = priority(offset, width, 0x400 + word * 32, word * 32) {
            return result.map(|mut decoded| {
                if let DecodedRegister::Model(ModelRegister {
                    kind: ModelRegisterKind::Priority { mask, .. },
                }) = &mut decoded
                {
                    *mask = 0xf8;
                }
                decoded
            });
        }
        if let Some(result) = configuration(offset, width, 0xc00 + word * 8, word * 32) {
            return result;
        }
    }
    if (0x800..0x800 + super::INTERRUPT_COUNT).contains(&offset) {
        let count = width.bytes() as u8;
        if !matches!(width, AccessWidth::Byte | AccessWidth::Word)
            || !offset.is_multiple_of(u32::from(count))
            || offset + u32::from(count) > 0x800 + super::INTERRUPT_COUNT
        {
            return Err(DecodeError::InvalidRegisterAccess);
        }
        return Ok(DecodedRegister::Model(ModelRegister {
            kind: ModelRegisterKind::Targets {
                first_interrupt: offset - 0x800,
                count,
            },
        }));
    }
    if (0xf10..0xf30).contains(&offset) {
        let count = width.bytes() as u8;
        if !matches!(width, AccessWidth::Byte | AccessWidth::Word)
            || !offset.is_multiple_of(u32::from(count))
        {
            return Err(DecodeError::InvalidRegisterAccess);
        }
        return Ok(DecodedRegister::Model(ModelRegister {
            kind: ModelRegisterKind::SgiPending {
                first_interrupt: (offset - 0xf10) % 16,
                count,
                set: offset >= 0xf20,
            },
        }));
    }
    if let Some(result) = model_word(
        offset,
        width,
        0xf00,
        ModelRegister {
            kind: ModelRegisterKind::SoftwareInterrupt,
        },
    ) {
        return result;
    }
    Ok(DecodedRegister::Reserved)
}

fn decode_frame(
    frame: Frame,
    offset: u64,
    width: AccessWidth,
) -> Result<DecodedAccess, DecodeError> {
    let offset = u32::try_from(offset).map_err(|_| DecodeError::CrossesFrame)?;
    let register = match frame {
        Frame::CpuDeactivateV2 => fixed(
            offset,
            width,
            0,
            AccessWidth::Word,
            ServiceRegister::CpuDeactivateV2,
        )
        .unwrap_or(Ok(DecodedRegister::Reserved))?,
        Frame::Distributor => decode_distributor(offset, width)?,
        Frame::DistributorV2 => decode_distributor_v2(offset, width)?,
        Frame::RedistributorControl => decode_redistributor_control(offset, width)?,
        Frame::RedistributorSgi => decode_redistributor_sgi(offset, width)?,
    };
    Ok(DecodedAccess {
        frame,
        register,
        redistributor: None,
    })
}

fn decode_distributor(offset: u32, width: AccessWidth) -> Result<DecodedRegister, DecodeError> {
    for (base, register) in [
        (0x0000, ServiceRegister::DistributorControl),
        (0x0004, ServiceRegister::DistributorType),
        (0x0008, ServiceRegister::DistributorImplementer),
        (0x000c, ServiceRegister::DistributorType2),
        (0x0010, ServiceRegister::DistributorStatus),
        (0xffe8, ServiceRegister::PeripheralId2),
    ] {
        if let Some(result) = fixed(offset, width, base, AccessWidth::Word, register) {
            return result;
        }
    }
    for (base, register) in [
        (0x0084, BitmapRegister::Group),
        (0x0104, BitmapRegister::SetEnable),
        (0x0184, BitmapRegister::ClearEnable),
        (0x0204, BitmapRegister::SetPending),
        (0x0284, BitmapRegister::ClearPending),
        (0x0304, BitmapRegister::SetActive),
        (0x0384, BitmapRegister::ClearActive),
    ] {
        for word in 0..super::INTERRUPT_COUNT / 32 - 1 {
            if let Some(result) = bitmap(offset, width, base + word * 4, 32 + word * 32, register) {
                return result;
            }
        }
    }
    for word in 0..super::INTERRUPT_COUNT / 32 - 1 {
        let first = 32 + word * 32;
        if let Some(result) = priority(offset, width, 0x0420 + word * 32, first) {
            return result;
        }
        if let Some(result) = configuration(offset, width, 0x0c08 + word * 8, first) {
            return result;
        }
        if let Some(result) = route(offset, width, 0x6100 + word * 256, first) {
            return result;
        }
    }
    Ok(DecodedRegister::Reserved)
}

fn decode_redistributor_control(
    offset: u32,
    width: AccessWidth,
) -> Result<DecodedRegister, DecodeError> {
    for (base, required, register) in [
        (
            0x0000,
            AccessWidth::Word,
            ServiceRegister::RedistributorControl,
        ),
        (
            0x0004,
            AccessWidth::Word,
            ServiceRegister::RedistributorImplementer,
        ),
        (
            0x0008,
            AccessWidth::DoubleWord,
            ServiceRegister::RedistributorType,
        ),
        (
            0x0010,
            AccessWidth::Word,
            ServiceRegister::RedistributorStatus,
        ),
        (
            0x0014,
            AccessWidth::Word,
            ServiceRegister::RedistributorWake,
        ),
        (0xffe8, AccessWidth::Word, ServiceRegister::PeripheralId2),
    ] {
        if let Some(result) = fixed(offset, width, base, required, register) {
            return result;
        }
    }
    Ok(DecodedRegister::Reserved)
}

fn decode_redistributor_sgi(
    offset: u32,
    width: AccessWidth,
) -> Result<DecodedRegister, DecodeError> {
    for (base, register) in [
        (0x0080, BitmapRegister::Group),
        (0x0100, BitmapRegister::SetEnable),
        (0x0180, BitmapRegister::ClearEnable),
        (0x0200, BitmapRegister::SetPending),
        (0x0280, BitmapRegister::ClearPending),
        (0x0300, BitmapRegister::SetActive),
        (0x0380, BitmapRegister::ClearActive),
    ] {
        if let Some(result) = bitmap(offset, width, base, 0, register) {
            return result;
        }
    }
    if let Some(result) = priority(offset, width, 0x0400, 0) {
        return result;
    }
    if let Some(result) = configuration(offset, width, 0x0c00, 0) {
        return result;
    }
    Ok(DecodedRegister::Reserved)
}

fn fixed(
    offset: u32,
    width: AccessWidth,
    base: u32,
    required: AccessWidth,
    register: ServiceRegister,
) -> Option<Result<DecodedRegister, DecodeError>> {
    if !overlaps(offset, width, base, required.bytes() as u32) {
        return None;
    }
    Some(if offset == base && width == required {
        Ok(DecodedRegister::Service(register))
    } else {
        Err(DecodeError::InvalidRegisterAccess)
    })
}

fn bitmap(
    offset: u32,
    width: AccessWidth,
    base: u32,
    first_interrupt: u32,
    register: BitmapRegister,
) -> Option<Result<DecodedRegister, DecodeError>> {
    model_word(
        offset,
        width,
        base,
        ModelRegister {
            kind: ModelRegisterKind::Bitmap {
                register,
                first_interrupt,
            },
        },
    )
}

fn model_word(
    offset: u32,
    width: AccessWidth,
    base: u32,
    register: ModelRegister,
) -> Option<Result<DecodedRegister, DecodeError>> {
    if !overlaps(offset, width, base, AccessWidth::Word.bytes() as u32) {
        return None;
    }
    Some(if offset == base && width == AccessWidth::Word {
        Ok(DecodedRegister::Model(register))
    } else {
        Err(DecodeError::InvalidRegisterAccess)
    })
}

fn priority(
    offset: u32,
    width: AccessWidth,
    base: u32,
    first_interrupt: u32,
) -> Option<Result<DecodedRegister, DecodeError>> {
    const PRIORITY_BYTES: u32 = 32;
    if !overlaps(offset, width, base, PRIORITY_BYTES) {
        return None;
    }
    let bytes = width.bytes() as u32;
    let Some(relative) = offset.checked_sub(base) else {
        return Some(Err(DecodeError::InvalidRegisterAccess));
    };
    let complete = relative
        .checked_add(bytes)
        .is_some_and(|end| end <= PRIORITY_BYTES);
    let valid = match width {
        AccessWidth::Byte => complete,
        AccessWidth::Word => complete && relative % 4 == 0,
        AccessWidth::HalfWord | AccessWidth::DoubleWord => false,
    };
    let first_lane = first_interrupt.checked_add(relative);
    Some(if valid && first_lane.is_some() {
        let Some(first_interrupt) = first_lane else {
            return Some(Err(DecodeError::InvalidRegisterAccess));
        };
        Ok(DecodedRegister::Model(ModelRegister {
            kind: ModelRegisterKind::Priority {
                first_interrupt,
                count: bytes as u8,
                mask: 0xff,
            },
        }))
    } else {
        Err(DecodeError::InvalidRegisterAccess)
    })
}

fn configuration(
    offset: u32,
    width: AccessWidth,
    base: u32,
    first_interrupt: u32,
) -> Option<Result<DecodedRegister, DecodeError>> {
    const CONFIGURATION_BYTES: u32 = 8;
    if !overlaps(offset, width, base, CONFIGURATION_BYTES) {
        return None;
    }
    let Some(relative) = offset.checked_sub(base) else {
        return Some(Err(DecodeError::InvalidRegisterAccess));
    };
    let valid = width == AccessWidth::Word && relative % 4 == 0 && relative < CONFIGURATION_BYTES;
    let lane_offset = (relative / 4).checked_mul(16);
    let first_lane = lane_offset.and_then(|offset| first_interrupt.checked_add(offset));
    Some(if valid && first_lane.is_some() {
        let Some(first_interrupt) = first_lane else {
            return Some(Err(DecodeError::InvalidRegisterAccess));
        };
        Ok(DecodedRegister::Model(ModelRegister {
            kind: ModelRegisterKind::Configuration { first_interrupt },
        }))
    } else {
        Err(DecodeError::InvalidRegisterAccess)
    })
}

fn route(
    offset: u32,
    width: AccessWidth,
    base: u32,
    first_interrupt: u32,
) -> Option<Result<DecodedRegister, DecodeError>> {
    const ROUTE_BYTES: u32 = 32 * 8;
    if !overlaps(offset, width, base, ROUTE_BYTES) {
        return None;
    }
    let Some(relative) = offset.checked_sub(base) else {
        return Some(Err(DecodeError::InvalidRegisterAccess));
    };
    // The reference interface requires aligned complete affinity-route accesses.
    let valid = width == AccessWidth::DoubleWord && relative % 8 == 0;
    let interrupt = first_interrupt.checked_add(relative / 8);
    Some(if valid && interrupt.is_some() {
        let Some(interrupt) = interrupt else {
            return Some(Err(DecodeError::InvalidRegisterAccess));
        };
        Ok(DecodedRegister::Model(ModelRegister {
            kind: ModelRegisterKind::Route(InterruptRoute { interrupt }),
        }))
    } else {
        Err(DecodeError::InvalidRegisterAccess)
    })
}

fn overlaps(offset: u32, width: AccessWidth, base: u32, bytes: u32) -> bool {
    let end = match offset.checked_add(width.bytes() as u32) {
        Some(end) => end,
        None => return true,
    };
    let register_end = match base.checked_add(bytes) {
        Some(end) => end,
        None => return true,
    };
    offset < register_end && end > base
}
