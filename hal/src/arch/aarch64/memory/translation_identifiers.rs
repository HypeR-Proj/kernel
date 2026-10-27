// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Decode independent ASID and VMID capabilities; VHE implies neither width.

use crate::arch::aarch64::registers;

pub(crate) const fn decode(mmfr0: u64, mmfr1: u64) -> Option<(u8, u8)> {
    let asid = match (mmfr0 >> registers::ID_AA64MMFR0_ASIDBITS_SHIFT) & 0xf {
        0 => 8,
        2 => 16,
        _ => return None,
    };
    let vmid = match (mmfr1 >> registers::ID_AA64MMFR1_VMIDBITS_SHIFT) & 0xf {
        0 => 8,
        2 => 16,
        _ => return None,
    };
    Some((asid, vmid))
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn independent_widths_and_reserved_encodings() {
        for (asid, asid_bits) in [(0, 8), (2, 16)] {
            for (vmid, vmid_bits) in [(0, 8), (2, 16)] {
                assert_eq!(decode(asid << 4, vmid << 4), Some((asid_bits, vmid_bits)));
            }
        }
        // VH is a different field and cannot imply VMID16 or ASID16.
        assert_eq!(decode(0, 1 << 8), Some((8, 8)));
        for reserved in [1, 3, 15] {
            assert_eq!(decode(reserved << 4, 0), None);
            assert_eq!(decode(0, reserved << 4), None);
        }
    }
}
