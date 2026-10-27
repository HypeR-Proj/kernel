// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime `AArch64` address-size capability selection.
//!
//! Build-time values describe the address-space policy. The boot CPU then
//! intersects the configured PA ceiling with `ID_AA64MMFR0_EL1.PARange` and
//! publishes one immutable TCR/VTCR configuration for every CPU.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::aarch64::registers;

pub const STAGE1_VA_BITS: u32 = hyper::config::ARM64_VA_BITS as u32;
pub const STAGE1_LAYOUT: crate::arch::aarch64::memory::address_layout::AddressLayout =
    match crate::arch::aarch64::memory::address_layout::AddressLayout::new(STAGE1_VA_BITS) {
        Some(layout) => layout,
        None => panic!("unsupported AArch64 virtual address layout"),
    };
pub const STAGE1_VA_LIMIT: u64 = STAGE1_LAYOUT.lower().size();
pub const CONFIGURED_PA_BITS: u32 = hyper::config::ARM64_PA_BITS as u32;
pub const STAGE2_IPA_BITS: u32 = hyper::config::ARM64_IPA_BITS as u32;
pub const STAGE2_IPA_LIMIT: u64 = 1_u64 << STAGE2_IPA_BITS;
pub const STAGE1_LEVELS: usize = 4;
pub const STAGE2_LEVELS: usize = 3;

const STATE_INITIALIZED: u64 = 1 << 63;
const STATE_SELECTED_PA_SHIFT: u32 = 0;
const STATE_SUPPORTED_PA_SHIFT: u32 = 8;
const STATE_PARANGE_SHIFT: u32 = 16;
const STATE_ASID_SHIFT: u32 = 24;
const STATE_VMID_SHIFT: u32 = 32;
const STATE_BYTE_MASK: u64 = 0xff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidPhysicalAddressRange,
    Unsupported4KGranule,
    UnsupportedIdentifierWidth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub virtual_address_bits: u8,
    pub physical_address_bits: u8,
    pub supported_physical_address_bits: u8,
    pub intermediate_physical_address_bits: u8,
    pub asid_bits: u8,
    pub vmid_bits: u8,
    parange: u8,
}

impl Capabilities {
    pub const fn stage1_levels(self) -> usize {
        STAGE1_LEVELS
    }

    pub const fn stage2_levels(self) -> usize {
        STAGE2_LEVELS
    }

    pub const fn physical_address_limit(self) -> u64 {
        1_u64 << self.physical_address_bits
    }

    pub const fn stage1_tcr_el2(self) -> u64 {
        registers::tcr_el2_vhe_stage1(self.virtual_address_bits, self.parange)
            | if self.asid_bits == 16 {
                registers::TCR_EL2_VHE_AS
            } else {
                0
            }
    }

    pub const fn stage2_vtcr_el2(self) -> u64 {
        registers::VTCR_EL2_GUEST_BASE
            | if self.vmid_bits == 16 {
                registers::VTCR_EL2_VS
            } else {
                0
            }
            | (64 - self.intermediate_physical_address_bits as u64)
            | ((self.parange as u64) << registers::VTCR_EL2_PS_SHIFT)
    }
}

static SELECTED: AtomicU64 = AtomicU64::new(0);

/// Selects and publishes the address-size policy on the boot CPU.
pub fn initialize() -> Result<Capabilities, Error> {
    let capabilities = select(read_id_aa64mmfr0_el1(), read_id_aa64mmfr1_el1())?;
    SELECTED.store(pack(capabilities), Ordering::Release);
    Ok(capabilities)
}

/// Returns the immutable address-size policy selected by the boot CPU.
pub fn capabilities() -> Capabilities {
    let state = SELECTED.load(Ordering::Acquire);
    assert!(
        state & STATE_INITIALIZED != 0,
        "AArch64 address policy is not initialized"
    );
    unpack(state)
}

pub fn physical_address_limit() -> u64 {
    capabilities().physical_address_limit()
}

/// Checks whether a secondary can safely use the boot CPU's translation regime.
pub fn current_cpu_is_compatible() -> bool {
    let selected = capabilities();
    let mmfr0 = read_id_aa64mmfr0_el1();
    let mmfr1 = read_id_aa64mmfr1_el1();
    hardware_capabilities(mmfr0).is_ok_and(|bits| bits >= selected.physical_address_bits)
        && crate::arch::aarch64::memory::translation_identifiers::decode(mmfr0, mmfr1)
            .is_some_and(|(asid, vmid)| asid >= selected.asid_bits && vmid >= selected.vmid_bits)
}

fn select(mmfr0: u64, mmfr1: u64) -> Result<Capabilities, Error> {
    let supported = hardware_capabilities(mmfr0)?;
    let selected = supported.min(CONFIGURED_PA_BITS as u8);
    let parange = parange_for_bits(selected).ok_or(Error::InvalidPhysicalAddressRange)?;
    let (asid_bits, vmid_bits) =
        crate::arch::aarch64::memory::translation_identifiers::decode(mmfr0, mmfr1)
            .ok_or(Error::UnsupportedIdentifierWidth)?;
    Ok(Capabilities {
        virtual_address_bits: STAGE1_VA_BITS as u8,
        physical_address_bits: selected,
        supported_physical_address_bits: supported,
        intermediate_physical_address_bits: STAGE2_IPA_BITS as u8,
        asid_bits,
        vmid_bits,
        parange,
    })
}

fn hardware_capabilities(mmfr0: u64) -> Result<u8, Error> {
    let granule = ((mmfr0 >> registers::ID_AA64MMFR0_TGRAN4_SHIFT)
        & registers::ID_AA64MMFR0_TGRAN4_MASK) as u8;
    if granule == registers::ID_AA64MMFR0_TGRAN4_UNSUPPORTED as u8 {
        return Err(Error::Unsupported4KGranule);
    }
    let parange = ((mmfr0 >> registers::ID_AA64MMFR0_PARANGE_SHIFT)
        & registers::ID_AA64MMFR0_PARANGE_MASK) as u8;
    bits_for_parange(parange).ok_or(Error::InvalidPhysicalAddressRange)
}

const fn bits_for_parange(parange: u8) -> Option<u8> {
    match parange as u64 {
        registers::ID_AA64MMFR0_PARANGE_32BIT => Some(32),
        registers::ID_AA64MMFR0_PARANGE_36BIT => Some(36),
        registers::ID_AA64MMFR0_PARANGE_40BIT => Some(40),
        registers::ID_AA64MMFR0_PARANGE_42BIT => Some(42),
        registers::ID_AA64MMFR0_PARANGE_44BIT => Some(44),
        registers::ID_AA64MMFR0_PARANGE_48BIT => Some(48),
        registers::ID_AA64MMFR0_PARANGE_52BIT => Some(52),
        _ => None,
    }
}

const fn parange_for_bits(bits: u8) -> Option<u8> {
    match bits {
        32 => Some(registers::ID_AA64MMFR0_PARANGE_32BIT as u8),
        36 => Some(registers::ID_AA64MMFR0_PARANGE_36BIT as u8),
        40 => Some(registers::ID_AA64MMFR0_PARANGE_40BIT as u8),
        42 => Some(registers::ID_AA64MMFR0_PARANGE_42BIT as u8),
        44 => Some(registers::ID_AA64MMFR0_PARANGE_44BIT as u8),
        48 => Some(registers::ID_AA64MMFR0_PARANGE_48BIT as u8),
        _ => None,
    }
}

const fn pack(capabilities: Capabilities) -> u64 {
    STATE_INITIALIZED
        | ((capabilities.physical_address_bits as u64) << STATE_SELECTED_PA_SHIFT)
        | ((capabilities.supported_physical_address_bits as u64) << STATE_SUPPORTED_PA_SHIFT)
        | ((capabilities.parange as u64) << STATE_PARANGE_SHIFT)
        | ((capabilities.asid_bits as u64) << STATE_ASID_SHIFT)
        | ((capabilities.vmid_bits as u64) << STATE_VMID_SHIFT)
}

const fn unpack(state: u64) -> Capabilities {
    Capabilities {
        virtual_address_bits: STAGE1_VA_BITS as u8,
        physical_address_bits: ((state >> STATE_SELECTED_PA_SHIFT) & STATE_BYTE_MASK) as u8,
        supported_physical_address_bits: ((state >> STATE_SUPPORTED_PA_SHIFT) & STATE_BYTE_MASK)
            as u8,
        intermediate_physical_address_bits: STAGE2_IPA_BITS as u8,
        asid_bits: ((state >> STATE_ASID_SHIFT) & STATE_BYTE_MASK) as u8,
        vmid_bits: ((state >> STATE_VMID_SHIFT) & STATE_BYTE_MASK) as u8,
        parange: ((state >> STATE_PARANGE_SHIFT) & STATE_BYTE_MASK) as u8,
    }
}

fn read_id_aa64mmfr0_el1() -> u64 {
    let value: u64;
    // SAFETY: ID_AA64MMFR0_EL1 is a read-only identification register at EL2.
    unsafe {
        asm!(
            "mrs {value}, ID_AA64MMFR0_EL1",
            value = out(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

fn read_id_aa64mmfr1_el1() -> u64 {
    let value: u64;
    // SAFETY: ID_AA64MMFR1_EL1 is a read-only identification register at EL2.
    unsafe {
        asm!("mrs {value}, ID_AA64MMFR1_EL1", value = out(reg) value,
            options(nomem, nostack, preserves_flags));
    }
    value
}
