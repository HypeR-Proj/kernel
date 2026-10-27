// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Small, allocation-free general-purpose PRNG. This is **not** a cryptographic
//! random source: do not use it for keys, secrets or authentication tokens.
//!
//! `SplitMix64` keeps eight bytes of state, permits every seed (including zero),
//! and performs no hardware access. Callers choose seed sources and serialize
//! shared instances; independent instances need no global lock.

use core::num::NonZeroU64;

/// A deterministic `SplitMix64` stream. Deliberately not `Clone`/`Copy`, to avoid
/// accidentally duplicating a running stream.
pub struct Random {
    state: u64,
}

impl Random {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Mixes an additional source. This diversifies the stream but neither
    /// estimates nor increases the source's actual entropy.
    pub fn mix(&mut self, seed: u64) {
        self.state = mix64(self.state ^ seed);
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix64(self.state)
    }

    /// Uniformly selects from `[0, upper)` using rejection rather than a biased
    /// remainder. The expected number of draws is less than two. Not an IRQ
    /// latency bound; callers needing bounded work can use `next_u64` directly.
    pub fn below(&mut self, upper: NonZeroU64) -> u64 {
        let upper = upper.get();
        let threshold = upper.wrapping_neg() % upper;
        loop {
            let value = self.next_u64();
            if value >= threshold {
                return value % upper;
            }
        }
    }

    /// Fills an arbitrary-sized buffer, including a final partial word. Byte
    /// order is fixed so seeded tests are reproducible on every architecture.
    pub fn fill_bytes(&mut self, output: &mut [u8]) {
        for chunk in output.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
