// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` guest instruction-cache alias policy.

const CTR_EL0_L1IP_SHIFT: u64 = 14;
const CTR_EL0_L1IP_MASK: u64 = 0b11;
const CTR_EL0_L1IP_PIPT: u64 = 0b11;
const CTR_EL0_IMINLINE_SHIFT: u64 = 0;
const CTR_EL0_DMINLINE_SHIFT: u64 = 16;
const CTR_EL0_LINE_SIZE_MASK: u64 = 0xf;
const CONTRACT_ALIAS_SHIFT: u64 = 8;

/// Invalidation scope needed after writing guest instructions through a host
/// virtual alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestInstructionCachePolicy {
    /// A physical-indexed cache permits precise invalidation by the host alias.
    Range,
    /// Aliasing or unknown indexing requires invalidating the complete
    /// inner-shareable instruction-cache domain.
    WholeInnerShareableDomain,
}

/// Selects the conservative guest instruction-cache policy from `CTR_EL0`.
///
/// Only the architecturally explicit PIPT encoding admits range invalidation.
/// VIPT, VPIPT, AIVIVT, and reserved/future behavior use whole-domain
/// invalidation because guest execution uses a different VA from the host
/// linear mapping used to populate the page.
pub const fn guest_instruction_cache_policy(ctr_el0: u64) -> GuestInstructionCachePolicy {
    if (ctr_el0 >> CTR_EL0_L1IP_SHIFT) & CTR_EL0_L1IP_MASK == CTR_EL0_L1IP_PIPT {
        GuestInstructionCachePolicy::Range
    } else {
        GuestInstructionCachePolicy::WholeInnerShareableDomain
    }
}

/// Returns the effective cache contract which must match across admitted PEs.
///
/// Data-line geometry is exact. Instruction-line geometry matters only for
/// PIPT caches, where publication uses range maintenance. All non-PIPT L1
/// instruction-cache encodings and line sizes are deliberately normalized to
/// the same conservative whole-domain policy.
pub const fn instruction_publication_contract(ctr_el0: u64) -> u64 {
    let data_line = (ctr_el0 >> CTR_EL0_DMINLINE_SHIFT) & CTR_EL0_LINE_SIZE_MASK;
    let (instruction_line, alias) = match guest_instruction_cache_policy(ctr_el0) {
        GuestInstructionCachePolicy::Range => (
            (ctr_el0 >> CTR_EL0_IMINLINE_SHIFT) & CTR_EL0_LINE_SIZE_MASK,
            1,
        ),
        GuestInstructionCachePolicy::WholeInnerShareableDomain => (0, 0),
    };
    instruction_line | (data_line << 4) | (alias << CONTRACT_ALIAS_SHIFT)
}
