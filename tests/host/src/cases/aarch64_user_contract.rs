// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host tests for the inert `AArch64` native-user machine contract.

use crate::aarch64_user_contract_model::{
    LowerElReturnRegime, UserExecutionCapabilities, UserMachineContractError, UserPagePermissions,
    UserTranslationRegime, UserTranslationRegisters,
};
use crate::registers;

fn nvhe_capabilities() -> UserExecutionCapabilities {
    UserExecutionCapabilities::new(UserTranslationRegime::NvheStage2Only, 39, 40, 8)
        .unwrap_or_else(|error| panic!("valid nVHE contract rejected: {error:?}"))
}

#[test]
fn absence_of_vhe_selects_a_supported_stage2_contract() {
    let capabilities = nvhe_capabilities();

    assert_eq!(capabilities.regime(), UserTranslationRegime::NvheStage2Only);
    assert_eq!(capabilities.address_bits(), 39);
    assert_eq!(capabilities.physical_address_bits(), 40);
    assert_eq!(capabilities.translation_identifier_bits(), 8);
}

#[test]
fn native_user_limit_respects_only_the_active_translation_regime() {
    let vhe = UserExecutionCapabilities::new(UserTranslationRegime::VheHostStage1, 48, 40, 8)
        .unwrap_or_else(|error| panic!("valid VHE contract rejected: {error:?}"));

    assert_eq!(vhe.user_address_limit(), 1 << 48);
    assert_eq!(nvhe_capabilities().user_address_limit(), 1 << 39);
}

#[test]
fn final_tcr_uses_the_host_mode_specific_physical_size_field() {
    let parange = registers::ID_AA64MMFR0_PARANGE_48BIT as u8;
    let nvhe = registers::tcr_el2_nvhe_stage1(48, parange);
    let vhe = registers::tcr_el2_vhe_stage1(48, parange);

    assert_eq!(
        (nvhe & registers::TCR_EL2_NVHE_PS_MASK) >> registers::TCR_EL2_NVHE_PS_SHIFT,
        u64::from(parange)
    );
    assert_eq!(
        (vhe & registers::TCR_EL2_VHE_IPS_MASK) >> registers::TCR_EL2_VHE_IPS_SHIFT,
        u64::from(parange)
    );
    assert_eq!(vhe & registers::TCR_EL2_VHE_EPD1, 0);
    assert_eq!(vhe & registers::TCR_EL2_T0SZ_MASK, 16);
    assert_eq!(
        (vhe >> registers::TCR_EL2_VHE_T1SZ_SHIFT) & registers::TCR_EL2_T0SZ_MASK,
        16
    );
}

#[test]
fn compact_upper_addresses_discard_sign_extension_at_the_root() {
    let upper_42_bit_base = 0u64.wrapping_sub(1 << 42);
    let upper_kernel_base = 0u64.wrapping_sub(1 << 40);

    assert_eq!(registers::stage1_table_index(upper_42_bit_base, 0, 42), 0);
    assert_eq!(registers::stage1_table_index(upper_kernel_base, 0, 42), 6);
    assert_eq!(registers::stage1_table_index(upper_kernel_base, 0, 48), 510);
}

#[test]
fn capability_limits_reject_unimplemented_register_formats() {
    assert_eq!(
        UserExecutionCapabilities::new(UserTranslationRegime::VheHostStage1, 49, 40, 8),
        Err(UserMachineContractError::UnsupportedAddressWidth)
    );
    assert_eq!(
        UserExecutionCapabilities::new(UserTranslationRegime::NvheStage2Only, 39, 52, 8),
        Err(UserMachineContractError::UnsupportedPhysicalAddressWidth)
    );
    assert_eq!(
        UserExecutionCapabilities::new(UserTranslationRegime::NvheStage2Only, 39, 40, 12),
        Err(UserMachineContractError::UnsupportedIdentifierWidth)
    );
}

#[test]
fn privileged_access_protection_has_a_distinct_admission_failure() {
    assert_ne!(
        UserMachineContractError::UnsupportedPrivilegedAccessProtection,
        UserMachineContractError::UnsupportedAddressWidth
    );
}

#[test]
fn translation_registers_reject_a_regime_mismatch() {
    assert_eq!(
        UserTranslationRegisters::new(
            nvhe_capabilities(),
            UserTranslationRegime::VheHostStage1,
            0x2000,
            1,
            1,
        ),
        Err(UserMachineContractError::TranslationRegimeMismatch)
    );
}

#[test]
fn translation_registers_validate_root_identity_and_generation() {
    let capabilities = nvhe_capabilities();

    assert_eq!(
        UserTranslationRegisters::new(
            capabilities,
            UserTranslationRegime::NvheStage2Only,
            0x2100,
            1,
            1,
        ),
        Err(UserMachineContractError::InvalidRootAlignment)
    );
    assert_eq!(
        UserTranslationRegisters::new(
            capabilities,
            UserTranslationRegime::NvheStage2Only,
            0x2000,
            0,
            1,
        ),
        Err(UserMachineContractError::InvalidTranslationIdentifier)
    );
    assert_eq!(
        UserTranslationRegisters::new(
            capabilities,
            UserTranslationRegime::NvheStage2Only,
            0x2000,
            1,
            0,
        ),
        Err(UserMachineContractError::InvalidGeneration)
    );
    assert_eq!(
        UserTranslationRegisters::new(
            capabilities,
            UserTranslationRegime::NvheStage2Only,
            1 << 40,
            1,
            1,
        ),
        Err(UserMachineContractError::RootOutsidePhysicalAddressSpace)
    );
}

#[test]
fn translation_register_encoding_retains_software_generation_separately() {
    let registers = UserTranslationRegisters::new(
        nvhe_capabilities(),
        UserTranslationRegime::NvheStage2Only,
        0x4000,
        7,
        19,
    )
    .unwrap_or_else(|error| panic!("valid translation registers rejected: {error:?}"));

    assert_eq!(
        registers.root_register(),
        (7 << registers::VTTBR_EL2_VMID_SHIFT) | 0x4000
    );
    assert_eq!(registers.generation(), 19);
    assert_eq!(registers.regime(), UserTranslationRegime::NvheStage2Only);
}

#[test]
fn vhe_register_encoding_uses_the_stage1_asid_field() {
    let capabilities =
        UserExecutionCapabilities::new(UserTranslationRegime::VheHostStage1, 48, 40, 8)
            .unwrap_or_else(|error| panic!("valid VHE contract rejected: {error:?}"));
    let registers = UserTranslationRegisters::new(
        capabilities,
        UserTranslationRegime::VheHostStage1,
        0x8000,
        3,
        5,
    )
    .unwrap_or_else(|error| panic!("valid VHE registers rejected: {error:?}"));

    assert_eq!(
        registers.root_register(),
        (3 << crate::registers::TTBR_ASID_SHIFT) | 0x8000
    );
}

#[test]
fn lower_el_vector_does_not_determine_the_vhe_return_world() {
    let host_hcr = registers::HCR_EL2_VHE_HOST_VALUE
        | registers::HCR_EL2_VM
        | registers::HCR_EL2_VI
        | registers::HCR_EL2_VF
        | registers::HCR_EL2_FB;
    let native = LowerElReturnRegime::Native(UserTranslationRegime::VheHostStage1)
        .transition_hcr(host_hcr)
        .unwrap_or_else(|error| panic!("valid native return rejected: {error:?}"));
    let guest = LowerElReturnRegime::Guest
        .transition_hcr(host_hcr)
        .unwrap_or_else(|error| panic!("valid guest return rejected: {error:?}"));

    assert_ne!(native & registers::HCR_EL2_TGE, 0);
    assert_eq!(native & registers::HCR_EL2_VM, 0);
    assert_eq!(
        native & (registers::HCR_EL2_VI | registers::HCR_EL2_VF | registers::HCR_EL2_FB),
        0,
        "native entry must not inherit guest virtual interrupt or broadcast state"
    );
    assert_eq!(guest & registers::HCR_EL2_TGE, 0);
    assert_ne!(guest & registers::HCR_EL2_VM, 0);
    assert_ne!(
        guest & registers::HCR_EL2_TWI,
        0,
        "guest entry must route WFI through the scheduler wait contract"
    );
    assert_ne!(
        guest & registers::HCR_EL2_TWE,
        0,
        "guest entry must route WFE through explicit exit handling"
    );
}

#[test]
fn nvhe_native_entry_forces_el1_stage1_translation_off() {
    assert_eq!(
        registers::SCTLR_EL1_GUEST_RESET_VALUE & registers::SCTLR_M,
        0
    );
}

#[test]
fn nvhe_native_and_guest_returns_have_different_cache_regimes() {
    let host_hcr = registers::HCR_EL2_BOOT_VALUE;
    let native = LowerElReturnRegime::Native(UserTranslationRegime::NvheStage2Only)
        .transition_hcr(host_hcr)
        .unwrap_or_else(|error| panic!("valid native return rejected: {error:?}"));
    let guest = LowerElReturnRegime::Guest
        .transition_hcr(native)
        .unwrap_or_else(|error| panic!("valid guest return rejected: {error:?}"));

    assert_ne!(native & registers::HCR_EL2_TGE, 0);
    assert_ne!(native & registers::HCR_EL2_DC, 0);
    assert_eq!(guest & registers::HCR_EL2_TGE, 0);
    assert_eq!(guest & registers::HCR_EL2_DC, 0);
}

#[test]
fn native_return_rejects_the_wrong_host_mode() {
    assert_eq!(
        LowerElReturnRegime::Native(UserTranslationRegime::VheHostStage1)
            .transition_hcr(registers::HCR_EL2_BOOT_VALUE),
        Err(UserMachineContractError::HostModeMismatch)
    );
    assert_eq!(
        LowerElReturnRegime::Native(UserTranslationRegime::NvheStage2Only)
            .transition_hcr(registers::HCR_EL2_VHE_HOST_VALUE),
        Err(UserMachineContractError::HostModeMismatch)
    );
}

#[test]
fn vhe_user_descriptors_enforce_el0_wx_and_privileged_execute_never() {
    let rw = UserPagePermissions::new(true, true, false)
        .unwrap_or_else(|error| panic!("valid RW permissions rejected: {error:?}"))
        .vhe_stage1_descriptor(0x1234_5000);
    assert_ne!(rw & registers::STAGE1_DESC_AP_EL0, 0);
    assert_ne!(rw & registers::STAGE1_DESC_NOT_GLOBAL, 0);
    assert_ne!(rw & registers::STAGE1_DESC_PXN, 0);
    assert_ne!(rw & registers::STAGE1_DESC_UXN, 0);
    assert_eq!(rw & registers::STAGE1_DESC_AP_READ_ONLY, 0);

    let rx = UserPagePermissions::new(true, false, true)
        .unwrap_or_else(|error| panic!("valid RX permissions rejected: {error:?}"))
        .vhe_stage1_descriptor(0x1234_5000);
    assert_ne!(rx & registers::STAGE1_DESC_AP_EL0, 0);
    assert_ne!(rx & registers::STAGE1_DESC_AP_READ_ONLY, 0);
    assert_ne!(rx & registers::STAGE1_DESC_PXN, 0);
    assert_eq!(rx & registers::STAGE1_DESC_UXN, 0);
}

#[test]
fn stage2_user_descriptors_reject_writable_execute_aliases() {
    assert_eq!(
        UserPagePermissions::new(true, true, true),
        Err(UserMachineContractError::InvalidPermissions)
    );
    let read_only = UserPagePermissions::new(true, false, false)
        .unwrap_or_else(|error| panic!("valid RO permissions rejected: {error:?}"))
        .nvhe_stage2_descriptor(0x8000);
    assert_ne!(read_only & registers::STAGE2_DESC_READ_ONLY, 0);
    assert_ne!(read_only & registers::STAGE2_DESC_XN, 0);
}
