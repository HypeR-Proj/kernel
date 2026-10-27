// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional Arm SMCCC TRNG firmware protocol (DEN0098). No pool, retries or
//! runtime polling: an unavailable sample is simply omitted from the boot seed.

const VERSION: u32 = 0x8400_0050;
const FEATURES: u32 = 0x8400_0051;
const RANDOM64: u32 = 0xc400_0053;

/// Requests 64 bits through an already valid firmware conduit. For a 64-bit
/// request, DEN0098 places the sample in x3, not x1. Check status before using it.
pub fn seed(mut invoke: impl FnMut(u32, u64) -> [u64; 4]) -> Option<u64> {
    if (invoke(VERSION, 0)[0] as u32) >> 16 != 1 {
        return None;
    }
    if invoke(FEATURES, u64::from(RANDOM64))[0] as u32 != 0 {
        return None;
    }
    let result = invoke(RANDOM64, 64);
    (result[0] as u32 == 0).then_some(result[3])
}
