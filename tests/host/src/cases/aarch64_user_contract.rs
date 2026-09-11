// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host tests for the inert `AArch64` native-user machine contract.

use crate::aarch64_user_contract_model::{
    LowerElReturnRegime, UserExecutionCapabilities, UserMachineContractError, UserPagePermissions,
    UserTranslationRegisters,
};
use crate::registers;

fn capabilities() -> UserExecutionCapabilities {
    UserExecutionCapabilities::new(39, 40, 8)
        .unwrap_or_else(|error| panic!("valid user contract rejected: {error:?}"))
}

#[test]
fn native_user_limit_respects_configured_address_width() {
    let vhe = UserExecutionCapabilities::new(48, 40, 8)
        .unwrap_or_else(|error| panic!("valid VHE contract rejected: {error:?}"));

    assert_eq!(vhe.user_address_limit(), 1 << 48);
    let compact = capabilities();
    assert_eq!(compact.user_address_limit(), 1 << 39);
}

#[test]
fn final_tcr_uses_the_vhe_physical_size_field() {
    let parange = registers::ID_AA64MMFR0_PARANGE_48BIT as u8;
    let vhe = registers::tcr_el2_vhe_stage1(48, parange);

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
        UserExecutionCapabilities::new(49, 40, 8),
        Err(UserMachineContractError::UnsupportedAddressWidth)
    );
    assert_eq!(
        UserExecutionCapabilities::new(39, 52, 8),
        Err(UserMachineContractError::UnsupportedPhysicalAddressWidth)
    );
    assert_eq!(
        UserExecutionCapabilities::new(39, 40, 12),
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
fn translation_registers_validate_root_identity_and_generation() {
    let capabilities = capabilities();

    assert_eq!(
        UserTranslationRegisters::new(capabilities, 0x2100, 1, 1,),
        Err(UserMachineContractError::InvalidRootAlignment)
    );
    assert_eq!(
        UserTranslationRegisters::new(capabilities, 0x2000, 0, 1,),
        Err(UserMachineContractError::InvalidTranslationIdentifier)
    );
    assert_eq!(
        UserTranslationRegisters::new(capabilities, 0x2000, 1, 0,),
        Err(UserMachineContractError::InvalidGeneration)
    );
    assert_eq!(
        UserTranslationRegisters::new(capabilities, 1 << 40, 1, 1,),
        Err(UserMachineContractError::RootOutsidePhysicalAddressSpace)
    );
}

#[test]
fn translation_register_encoding_retains_software_generation_separately() {
    let registers = UserTranslationRegisters::new(capabilities(), 0x4000, 7, 19)
        .unwrap_or_else(|error| panic!("valid translation registers rejected: {error:?}"));

    assert_eq!(
        registers.root_register(),
        (7 << registers::TTBR_ASID_SHIFT) | 0x4000
    );
    assert_eq!(registers.generation(), 19);
}

#[test]
fn vhe_register_encoding_uses_the_stage1_asid_field() {
    let capabilities = UserExecutionCapabilities::new(48, 40, 8)
        .unwrap_or_else(|error| panic!("valid VHE contract rejected: {error:?}"));
    let registers = UserTranslationRegisters::new(capabilities, 0x8000, 3, 5)
        .unwrap_or_else(|error| panic!("valid VHE registers rejected: {error:?}"));

    assert_eq!(
        registers.root_register(),
        (3 << crate::registers::TTBR_ASID_SHIFT) | 0x8000
    );
}

#[test]
fn lower_el_vector_does_not_determine_the_vhe_return_world() {
    // HCR_EL2[11] is RES0 in the admitted AArch64 register contract. Keep it
    // in the contaminated input so explicit guest policy cannot accidentally
    // widen BSU into a two-bit field.
    const HCR_EL2_RES0_11: u64 = 1 << 11;
    let host_hcr = registers::HCR_EL2_VHE_HOST_VALUE
        | registers::HCR_EL2_VM
        | registers::HCR_EL2_VI
        | registers::HCR_EL2_VF
        | registers::HCR_EL2_FB
        | registers::HCR_EL2_DC
        | registers::HCR_EL2_TID0
        | registers::HCR_EL2_TID1
        | registers::HCR_EL2_TID2
        | registers::HCR_EL2_TSW
        | registers::HCR_EL2_TPCP
        | registers::HCR_EL2_TPU
        | registers::HCR_EL2_TTLB
        | HCR_EL2_RES0_11;
    let native = LowerElReturnRegime::Native
        .transition_hcr(host_hcr)
        .unwrap_or_else(|error| panic!("valid native return rejected: {error:?}"));
    let guest = LowerElReturnRegime::Guest
        .transition_hcr(host_hcr & !registers::HCR_EL2_FB)
        .unwrap_or_else(|error| panic!("valid guest return rejected: {error:?}"));
    let expected_guest = registers::HCR_EL2_E2H
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
        | registers::HCR_EL2_BSU_IS;

    assert_ne!(native & registers::HCR_EL2_TGE, 0);
    assert_eq!(guest, expected_guest, "guest HCR must be explicit policy");
    assert_eq!(registers::HCR_EL2_BSU_MASK & HCR_EL2_RES0_11, 0);
    assert_eq!(native & registers::HCR_EL2_VM, 0);
    assert_eq!(
        native & (registers::HCR_EL2_VI | registers::HCR_EL2_VF | registers::HCR_EL2_FB),
        0,
        "native entry must not inherit guest virtual interrupt or broadcast state"
    );
    assert_eq!(guest & registers::HCR_EL2_TGE, 0);
    assert_ne!(guest & registers::HCR_EL2_VM, 0);
    assert_ne!(
        guest & registers::HCR_EL2_E2H,
        0,
        "guest entry must preserve the host's VHE register regime"
    );
    assert_ne!(
        guest & registers::HCR_EL2_FB,
        0,
        "migratable guests require broadcast guest cache/TLB maintenance"
    );
    assert_eq!(
        guest & registers::HCR_EL2_BSU_MASK,
        registers::HCR_EL2_BSU_IS,
        "guest barriers must complete broadcast maintenance in the inner-shareable domain"
    );
    assert_eq!(
        guest & (registers::HCR_EL2_SWIO | registers::HCR_EL2_PTW),
        registers::HCR_EL2_SWIO | registers::HCR_EL2_PTW,
        "guest cache and table-walk policy must preserve host memory integrity"
    );
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
    assert_ne!(
        guest & registers::HCR_EL2_TID3,
        0,
        "guest feature-ID reads must route through the sanitized virtual CPU model"
    );
    assert_eq!(
        guest & (registers::HCR_EL2_TSC | registers::HCR_EL2_TACR | registers::HCR_EL2_TIDCP),
        registers::HCR_EL2_TSC | registers::HCR_EL2_TACR | registers::HCR_EL2_TIDCP,
        "guest firmware and implementation-defined accesses must remain virtualized"
    );
    assert_eq!(
        guest
            & (registers::HCR_EL2_TID0
                | registers::HCR_EL2_TID1
                | registers::HCR_EL2_TID2
                | registers::HCR_EL2_TSW
                | registers::HCR_EL2_TPCP
                | registers::HCR_EL2_TPU
                | registers::HCR_EL2_TTLB),
        0,
        "native-only trap policy must not leak into a guest"
    );
}

#[test]
fn native_return_rejects_the_wrong_host_mode() {
    assert_eq!(
        LowerElReturnRegime::Native.transition_hcr(registers::HCR_EL2_BOOT_VALUE),
        Err(UserMachineContractError::HostModeMismatch)
    );
}

#[test]
fn vhe_user_descriptors_enforce_el0_wx_and_privileged_execute_never() {
    let rw = UserPagePermissions::new(true, true, false)
        .unwrap_or_else(|error| panic!("valid RW permissions rejected: {error:?}"))
        .stage1_descriptor(0x1234_5000);
    assert_ne!(rw & registers::STAGE1_DESC_AP_EL0, 0);
    assert_ne!(rw & registers::STAGE1_DESC_NOT_GLOBAL, 0);
    assert_ne!(rw & registers::STAGE1_DESC_PXN, 0);
    assert_ne!(rw & registers::STAGE1_DESC_UXN, 0);
    assert_eq!(rw & registers::STAGE1_DESC_AP_READ_ONLY, 0);

    let rx = UserPagePermissions::new(true, false, true)
        .unwrap_or_else(|error| panic!("valid RX permissions rejected: {error:?}"))
        .stage1_descriptor(0x1234_5000);
    assert_ne!(rx & registers::STAGE1_DESC_AP_EL0, 0);
    assert_ne!(rx & registers::STAGE1_DESC_AP_READ_ONLY, 0);
    assert_ne!(rx & registers::STAGE1_DESC_PXN, 0);
    assert_eq!(rx & registers::STAGE1_DESC_UXN, 0);
}

#[test]
fn user_descriptors_reject_writable_execute_aliases() {
    assert_eq!(
        UserPagePermissions::new(true, true, true),
        Err(UserMachineContractError::InvalidPermissions)
    );
    let read_only = UserPagePermissions::new(true, false, false)
        .unwrap_or_else(|error| panic!("valid RO permissions rejected: {error:?}"))
        .stage1_descriptor(0x8000);
    assert_ne!(read_only & registers::STAGE1_DESC_AP_READ_ONLY, 0);
    assert_ne!(read_only & registers::STAGE1_DESC_UXN, 0);
}

#[test]
fn cow_fault_classification_requires_a_user_store_permission_fault() {
    use crate::aarch64_user_contract_model::is_user_write_page_fault;
    let class = registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT;
    for level in 0..4 {
        let fault = class | registers::ESR_DATA_ABORT_WNR | (0b001100 + level);
        assert!(is_user_write_page_fault(fault));
        assert!(!is_user_write_page_fault(
            fault & !registers::ESR_DATA_ABORT_WNR
        ));
        for excluded in [registers::ESR_DATA_ABORT_S1PTW, 1 << 8, 1 << 10] {
            assert!(!is_user_write_page_fault(fault | excluded));
        }
    }
    for status in 0..64 {
        if !(12..=15).contains(&status) {
            assert!(!is_user_write_page_fault(
                class | registers::ESR_DATA_ABORT_WNR | status
            ));
        }
    }
    let instruction = registers::ESR_EC_INSTRUCTION_ABORT_LOWER << registers::ESR_EC_SHIFT;
    assert!(!is_user_write_page_fault(
        instruction | registers::ESR_DATA_ABORT_WNR | 15
    ));
}
