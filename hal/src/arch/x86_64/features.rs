// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::x86_64::__cpuid_count;

pub fn running_under_qemu_tcg() -> bool {
    let basic = __cpuid_count(1, 0);
    if basic.ecx & (1 << 31) == 0 {
        return false;
    }
    let vendor = __cpuid_count(0x4000_0000, 0);
    vendor.ebx == 0x5447_4354 && vendor.ecx == 0x4354_4743 && vendor.edx == 0x4743_5447
}

pub fn tsc_frequency() -> Option<u64> {
    let maximum = __cpuid_count(0, 0).eax;
    if maximum >= 0x15 {
        let leaf = __cpuid_count(0x15, 0);
        if leaf.eax != 0 && leaf.ebx != 0 && leaf.ecx != 0 {
            return u64::from(leaf.ecx)
                .checked_mul(u64::from(leaf.ebx))?
                .checked_div(u64::from(leaf.eax));
        }
    }
    if maximum >= 0x16 {
        let leaf = __cpuid_count(0x16, 0);
        if leaf.eax != 0 {
            return u64::from(leaf.eax).checked_mul(1_000_000);
        }
    }
    None
}
