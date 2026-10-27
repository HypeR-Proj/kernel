// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Machine contracts for native `AArch64` userspace.
//!
//! This module validates register encodings and makes lower-EL return regimes
//! explicit. It deliberately owns no page-table memory or residency policy;
//! those owners remain in the kernel and reach the architecture only through
//! opaque HAL capabilities.

use crate::arch::aarch64::registers;
use hyper::hal::user::UserFaultKind;

const MINIMUM_ADDRESS_BITS: u8 = 32;
const MAXIMUM_ADDRESS_BITS: u8 = 48;

/// Immutable machine limits selected during boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserExecutionCapabilities {
    address_bits: u8,
    physical_address_bits: u8,
    translation_identifier_bits: u8,
}

impl UserExecutionCapabilities {
    /// Validates one host-selected native-user translation contract.
    pub(crate) const fn new(
        address_bits: u8,
        physical_address_bits: u8,
        translation_identifier_bits: u8,
    ) -> Result<Self, UserMachineContractError> {
        if address_bits < MINIMUM_ADDRESS_BITS || address_bits > MAXIMUM_ADDRESS_BITS {
            return Err(UserMachineContractError::UnsupportedAddressWidth);
        }
        if physical_address_bits < MINIMUM_ADDRESS_BITS
            || physical_address_bits > MAXIMUM_ADDRESS_BITS
        {
            return Err(UserMachineContractError::UnsupportedPhysicalAddressWidth);
        }
        if translation_identifier_bits != 8 && translation_identifier_bits != 16 {
            return Err(UserMachineContractError::UnsupportedIdentifierWidth);
        }
        Ok(Self {
            address_bits,
            physical_address_bits,
            translation_identifier_bits,
        })
    }

    /// Returns the exclusive native-user limit of the selected translation regime.
    pub(crate) const fn user_address_limit(self) -> u64 {
        1u64 << self.address_bits
    }
}

/// Inert register values for one process address space.
///
/// The generation prevents a future residency token from silently referring
/// to a recycled software address-space identity. It is not an architectural
/// ASID and is never encoded into a register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserTranslationRegisters {
    root_register: u64,
}

impl UserTranslationRegisters {
    /// Validates the root and identifier without claiming root ownership.
    ///
    /// The returned value is data only. Activation remains a separate HAL
    /// operation which requires the kernel owner to retain the hierarchy and
    /// identifier while its residency protocol admits the calling CPU.
    pub(crate) const fn new(
        capabilities: UserExecutionCapabilities,
        root_address: u64,
        translation_identifier: u16,
        generation: u64,
    ) -> Result<Self, UserMachineContractError> {
        if root_address & (registers::TRANSLATION_GRANULE_4K - 1) != 0 {
            return Err(UserMachineContractError::InvalidRootAlignment);
        }
        let physical_limit = 1_u64 << capabilities.physical_address_bits;
        if root_address >= physical_limit
            || root_address & !registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT != 0
        {
            return Err(UserMachineContractError::RootOutsidePhysicalAddressSpace);
        }
        if translation_identifier == 0
            || translation_identifier as u32 >= (1_u32 << capabilities.translation_identifier_bits)
        {
            return Err(UserMachineContractError::InvalidTranslationIdentifier);
        }
        if generation == 0 {
            return Err(UserMachineContractError::InvalidGeneration);
        }
        Ok(Self {
            root_register: ((translation_identifier as u64) << registers::TTBR_ASID_SHIFT)
                | root_address,
        })
    }

    pub(crate) const fn root_register(self) -> u64 {
        self.root_register
    }
}

/// Explicit lower-EL world selected by an owned return capability.
///
/// A lower-AArch64 exception vector does not identify this state. Native and
/// guest execution use the same vector slots but require different `HCR_EL2`
/// values; Native execution admits EL0 while guest execution admits EL1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LowerElReturnRegime {
    Native,
    #[cfg_attr(not(test), allow(dead_code))]
    Guest,
}

impl LowerElReturnRegime {
    /// Produces the `HCR_EL2` value required before `ERET`.
    ///
    /// Native and guest values are both built from explicit policy rather than
    /// inherited from the preceding lower-EL context. Both require the host
    /// EL2&0 register regime (`E2H=1`), while only Native sets `TGE`.
    pub(crate) const fn transition_hcr(
        self,
        current_hcr: u64,
    ) -> Result<u64, UserMachineContractError> {
        if current_hcr & registers::HCR_EL2_E2H == 0 {
            return Err(UserMachineContractError::HostModeMismatch);
        }
        Ok(match self {
            Self::Native => native_hcr_base() | registers::HCR_EL2_E2H | registers::HCR_EL2_TGE,
            Self::Guest => Self::guest_hcr(),
        })
    }

    pub(crate) const fn guest_hcr() -> u64 {
        // FB broadcasts guest cache and translation maintenance, while BSU_IS
        // upgrades the completing guest barriers to the same inner-shareable
        // domain. Without the pair, a migratable vCPU can leave stale guest
        // translations or instructions on a physical PE and later return to
        // them. SWIO prevents guest set/way invalidation from discarding dirty
        // cache state, and PTW keeps guest table walks subject to stage-2 write
        // permission.
        // The host always retains E2H. TID3 is guest policy: it routes
        // the feature-ID register family through the sanitized virtual CPU
        // model. TSC keeps guest SMCs out of EL3, while TACR and TIDCP prevent
        // implementation-defined EL1 controls from exposing host policy; the
        // virtual system-register path returns the supported ACTLR contract and
        // injects Undefined Instruction for unknown accesses. The remaining
        // native-EL0 discovery, cache-maintenance, and TLB-maintenance traps
        // must not leak through a scheduler transition into Linux EL1.
        registers::HCR_EL2_E2H
            | registers::HCR_EL2_VM
            | registers::HCR_EL2_SWIO
            | registers::HCR_EL2_PTW
            | registers::HCR_EL2_RW
            | registers::HCR_EL2_FMO
            | registers::HCR_EL2_IMO
            | registers::HCR_EL2_AMO
            | registers::HCR_EL2_TWI
            | registers::HCR_EL2_TWE
            | registers::HCR_EL2_TID3
            | registers::HCR_EL2_TSC
            | registers::HCR_EL2_TACR
            | registers::HCR_EL2_TIDCP
            | registers::HCR_EL2_FB
            | registers::HCR_EL2_BSU_IS
    }
}

/// Deterministic trap policy for untrusted native EL0 execution.
///
/// Unsupported wait, cache-maintenance, translation-maintenance, and feature
/// discovery operations trap instead of inheriting controls from whichever
/// guest or native thread previously occupied the CPU.
const fn native_hcr_base() -> u64 {
    registers::HCR_EL2_BOOT_VALUE
        | registers::HCR_EL2_TWI
        | registers::HCR_EL2_TWE
        | registers::HCR_EL2_TID0
        | registers::HCR_EL2_TID1
        | registers::HCR_EL2_TID2
        | registers::HCR_EL2_TID3
        | registers::HCR_EL2_TSC
        | registers::HCR_EL2_TIDCP
        | registers::HCR_EL2_TSW
        | registers::HCR_EL2_TPCP
        | registers::HCR_EL2_TPU
        | registers::HCR_EL2_TTLB
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserMachineContractError {
    HostModeMismatch,
    InvalidGeneration,
    InvalidPermissions,
    InvalidRootAlignment,
    InvalidTranslationIdentifier,
    RootOutsidePhysicalAddressSpace,
    UnsupportedAddressWidth,
    UnsupportedIdentifierWidth,
    UnsupportedPhysicalAddressWidth,
    UnsupportedPrivilegedAccessProtection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserPagePermissions {
    readable: bool,
    writable: bool,
    executable: bool,
}

impl UserPagePermissions {
    pub(crate) const fn new(
        readable: bool,
        writable: bool,
        executable: bool,
    ) -> Result<Self, UserMachineContractError> {
        if (writable || executable) && !readable || (writable && executable) {
            return Err(UserMachineContractError::InvalidPermissions);
        }
        Ok(Self {
            readable,
            writable,
            executable,
        })
    }

    pub(crate) const fn stage1_descriptor(self, physical: u64) -> u64 {
        if !self.readable {
            return registers::STAGE1_DESC_INVALID;
        }
        let mut descriptor = (physical & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT)
            | registers::STAGE1_DESC_TABLE_OR_PAGE
            | registers::STAGE1_DESC_ATTR_NORMAL
            | registers::STAGE1_DESC_INNER_SHAREABLE
            | registers::STAGE1_DESC_AP_EL0
            | registers::STAGE1_DESC_ACCESS_FLAG
            | registers::STAGE1_DESC_NOT_GLOBAL
            | registers::STAGE1_DESC_PXN;
        if !self.writable {
            descriptor |= registers::STAGE1_DESC_AP_READ_ONLY;
        }
        if !self.executable {
            descriptor |= registers::STAGE1_DESC_UXN;
        }
        descriptor
    }
}

/// Classifies a captured EL0 fault without accessing machine or mapping state.
///
/// Missing translations may be backed by pageable files. Only a *write*
/// permission fault may instead need a private COW page; read/execute permission
/// failures and access-flag faults cannot be repaired by the file pager. The
/// kernel must still validate mapping permissions and require progress before
/// retrying any page-fault candidate returned here.
pub(crate) const fn fault_kind(syndrome: u64) -> UserFaultKind {
    let class = (syndrome >> registers::ESR_EC_SHIFT) & registers::ESR_EC_MASK;
    let status = syndrome & registers::ESR_ABORT_FSC_MASK;
    let translation = status >= registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL0
        && status <= registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL3;
    let permission = status >= registers::ESR_ABORT_PERMISSION_FAULT_LEVEL0
        && status <= registers::ESR_ABORT_PERMISSION_FAULT_LEVEL3;
    let excluded =
        registers::ESR_DATA_ABORT_S1PTW | registers::ESR_DATA_ABORT_CM | registers::ESR_ABORT_FNV;
    // Table walks and cache maintenance are not ordinary user memory accesses;
    // FnV also makes the recorded virtual address unusable for paging.
    if syndrome & excluded == 0 {
        if class == registers::ESR_EC_INSTRUCTION_ABORT_LOWER && translation {
            return UserFaultKind::ExecutePageFault;
        }
        if class == registers::ESR_EC_DATA_ABORT_LOWER {
            let write = syndrome & registers::ESR_DATA_ABORT_WNR != 0;
            if write && (translation || permission) {
                return UserFaultKind::WritePageFault;
            }
            if !write && translation {
                return UserFaultKind::ReadPageFault;
            }
        }
    }
    match class {
        registers::ESR_EC_INSTRUCTION_ABORT_LOWER => UserFaultKind::InstructionAbort,
        registers::ESR_EC_DATA_ABORT_LOWER => UserFaultKind::DataAbort,
        registers::ESR_EC_PC_ALIGNMENT | registers::ESR_EC_SP_ALIGNMENT => UserFaultKind::Alignment,
        registers::ESR_EC_SYSTEM_REGISTER => UserFaultKind::SystemAccess,
        registers::ESR_EC_BREAKPOINT_LOWER
        | registers::ESR_EC_SOFTWARE_STEP_LOWER
        | registers::ESR_EC_WATCHPOINT_LOWER
        | registers::ESR_EC_BRK64 => UserFaultKind::Breakpoint,
        registers::ESR_EC_UNKNOWN => UserFaultKind::IllegalInstruction,
        _ => UserFaultKind::OtherSynchronous,
    }
}
