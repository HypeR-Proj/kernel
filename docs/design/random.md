<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# General-purpose kernel random numbers

This is a small **non-cryptographic** PRNG for ordinary kernel use. It does not
provide an entropy pool, a security readiness guarantee, or a userspace random
syscall. Do not use its output for keys, authentication tokens or secrets.

`hyper::random::Random` implements SplitMix64 with eight bytes of state and no
allocation or dependencies. A caller-owned instance provides `next_u64`,
`fill_bytes`, and unbiased `below(NonZeroU64)` selection. A fixed seed gives a
reproducible stream for tests; each draw advances the state even when no further
seed material arrives. Partial-word fills consume a complete word.

The binary's `crate::random` module owns the shared kernel instance. A short
IRQ-masked lock serializes draws on multiple CPUs. Bulk fills acquire it for at most 64 bytes
at a time; they never copy to userspace under this lock. There is no timer,
interrupt sampling hook, worker, or periodic reseeding cost.

Process creation draws one seed for its handle table's fixed numeric mask.
Handle operations do not draw random numbers or acquire the PRNG lock.

Before KASLR and SMP, boot mixes the boot counter, `/chosen/kaslr-seed`, optional
`/chosen/rng-seed`, and one optional hardware sample. `rng-seed` accepts 1–256
bytes, folds them into a word, and rejects duplicate or invalid properties
without rejecting the rest of the device tree. Folding is not a cryptographic
hash or an estimate of entropy. A fixed boot image on a deterministic emulator
can still produce a deterministic stream.

On AArch64, the optional hardware source first probes the Arm SMCCC TRNG service
when the platform advertises a standard SMC firmware conduit. A successful
64-bit request uses x3. Unsupported firmware or unavailable entropy is skipped;
there is no polling. FEAT_RNG/RNDR, when advertised, is the fallback. Other
architectures currently use the boot inputs without a hardware RNG probe.
Availability of ATF alone does not imply that its platform implements TRNG.

The firmware protocol driver lives in `drivers/src/random/trng.rs`, exposed as
`hyper_drivers::random::trng`. The AArch64 adapter in
`hal/src/arch/aarch64/random.rs` supplies the firmware conduit and selects the
hardware fallback; the portable PRNG remains in `core/src/random.rs`.

KASLR uses the same PRNG implementation and the combined boot seed, without a
private random mixing algorithm. `nokaslr` and the build configuration still
disable address randomization. Timing alone does not enable KASLR: without a
bootloader or hardware seed it keeps the baseline layout. Sharing a lightweight
PRNG does not make KASLR or the general random API cryptographically secure.

Algorithm reference: [SplitMix64](https://prng.di.unimi.it/splitmix64.c).
Firmware interface: [Arm TRNG service in TF-A](https://github.com/ARM-software/arm-trusted-firmware/blob/master/services/std_svc/trng/trng_main.c).
