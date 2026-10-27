// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest identifier policy when hardware cannot tag the supported VM population.

/// Provide a uniform minimum of 255 logical tags for the registry's capacity
/// proof. Narrower hardware uses untagged execution with mandatory switch
/// fences, so VM admission does not depend on its smaller hardware namespace.
pub(crate) const fn guest_software_bits(hardware_bits: u8) -> u8 {
    if hardware_bits < 8 { 8 } else { hardware_bits }
}

pub(crate) const fn guest_hardware_identifier(hardware_bits: u8, logical: u16) -> u16 {
    if hardware_bits < 8 { 0 } else { logical }
}

pub(crate) const fn guest_selection_cacheable(hardware_bits: u8) -> bool {
    hardware_bits >= 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_namespaces_require_untagged_switches() {
        for bits in 0..8 {
            assert_eq!(guest_software_bits(bits), 8);
            assert!(!guest_selection_cacheable(bits));
            for logical in [1, 63, 64, 255] {
                assert_eq!(guest_hardware_identifier(bits, logical), 0);
            }
        }
    }

    #[test]
    fn sufficient_hardware_preserves_tag_and_selection() {
        for bits in 8..=14 {
            assert_eq!(guest_software_bits(bits), bits);
            assert!(guest_selection_cacheable(bits));
            let last = (1_u16 << bits) - 1;
            assert_eq!(guest_hardware_identifier(bits, last), last);
        }
    }
}
