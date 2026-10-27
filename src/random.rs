// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared, non-cryptographic kernel PRNG. Boot seeds it before KASLR/SMP.
//! Each draw advances its state; there is no periodic or interrupt-path work.

use hyper::{random::Random, sync::InterruptSpinLock};

static RANDOM: InterruptSpinLock<Random, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(Random::new(0));

/// Runs once during boot, before any consumers or interrupts are admitted.
pub(crate) fn initialize(kaslr: Option<u64>, boot: Option<u64>, hardware: Option<u64>) {
    RANDOM.with(|random| {
        random.mix(crate::hal::time::boot_counter());
        for seed in [kaslr, boot, hardware].into_iter().flatten() {
            random.mix(seed);
        }
    });
}

/// Fixed work under the IRQ-masked lock, with no allocation or hardware access.
/// This API must not be used for cryptographic secrets.
pub fn next_u64() -> u64 {
    RANDOM.with(Random::next_u64)
}

/// Keeps bulk generation from masking IRQs for an unbounded caller-sized buffer.
pub fn fill_bytes(output: &mut [u8]) {
    for chunk in output.chunks_mut(64) {
        RANDOM.with(|random| random.fill_bytes(chunk));
    }
}
