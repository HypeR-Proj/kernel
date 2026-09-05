// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! AArch64 guest instruction-cache alias policy.

use hyper::vm::aarch64::cache::{
    GuestInstructionCachePolicy, guest_instruction_cache_policy, instruction_publication_contract,
};

#[test]
fn pipt_admits_precise_guest_instruction_invalidation() {
    let ctr = 0b11_u64 << 14;
    assert_eq!(
        guest_instruction_cache_policy(ctr),
        GuestInstructionCachePolicy::Range
    );
}

#[test]
fn every_non_pipt_encoding_uses_conservative_domain_invalidation() {
    for encoding in 0_u64..0b11 {
        let ctr = encoding << 14;
        assert_eq!(
            guest_instruction_cache_policy(ctr),
            GuestInstructionCachePolicy::WholeInnerShareableDomain
        );
    }
}

#[test]
fn publication_contract_rejects_line_mismatch_but_normalizes_aliasing_caches() {
    let conservative = (4_u64 << 16) | 4;
    let another_conservative_alias = conservative | (0b10 << 14);
    let conservative_with_another_instruction_line = (4_u64 << 16) | 5;
    let pipt = conservative | (0b11 << 14);
    let pipt_with_another_instruction_line =
        conservative_with_another_instruction_line | (0b11 << 14);
    let different_data_line = (5_u64 << 16) | 4;

    assert_eq!(
        instruction_publication_contract(conservative),
        instruction_publication_contract(another_conservative_alias)
    );
    assert_eq!(
        instruction_publication_contract(conservative),
        instruction_publication_contract(conservative_with_another_instruction_line)
    );
    assert_ne!(
        instruction_publication_contract(conservative),
        instruction_publication_contract(pipt)
    );
    assert_ne!(
        instruction_publication_contract(conservative),
        instruction_publication_contract(different_data_line)
    );
    assert_ne!(
        instruction_publication_contract(pipt),
        instruction_publication_contract(pipt_with_another_instruction_line)
    );
}
