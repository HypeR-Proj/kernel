// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Non-faulting copies of externally mutable Normal memory. This is not an
//! MMIO accessor, an atomic snapshot, or a publication barrier.

use core::arch::asm;

/// Copy between disjoint resident ranges without giving LLVM a Rust-memory
/// exclusivity assumption about the machine-visible side.
///
/// # Safety
///
/// Both pointers must be valid for `length` bytes of non-faulting Normal memory.
/// The destination is writable and the ranges do not overlap. Either side may
/// be accessed by another PE, but the caller must independently synchronize any
/// protocol that requires stable data. No access extends outside these ranges.
/// An asynchronous machine failure may leave a partially copied destination.
#[inline]
pub(crate) unsafe fn copy(source: *const u8, destination: *mut u8, length: usize) {
    // SAFETY: The caller owns the resident mappings and supplies disjoint valid
    // ranges. Paired 64-bit accesses are used only when both addresses are
    // 8-byte aligned and at least 32 bytes remain. The word and byte tails never
    // overread or overwrite. Unaligned buffers retain the byte path.
    // Only GPRs are used: no kernel FP/SIMD context is borrowed. One asm block
    // with a compiler memory clobber prevents memcpy substitution and claims
    // neither a Rust shared reference nor atomicity for machine-visible bytes.
    unsafe {
        asm!(
            "cbz {length}, 7f",
            "orr {a}, {source}, {destination}",
            "tst {a}, #7",
            "b.ne 5f",
            "cmp {length}, #32",
            "b.lo 3f",
            "2:",
            "ldp {a}, {b}, [{source}]",
            "ldp {c}, {d}, [{source}, #16]",
            "stp {a}, {b}, [{destination}]",
            "stp {c}, {d}, [{destination}, #16]",
            "add {source}, {source}, #32",
            "add {destination}, {destination}, #32",
            "sub {length}, {length}, #32",
            "cmp {length}, #32",
            "b.hs 2b",
            "3:",
            "cmp {length}, #8",
            "b.lo 5f",
            "4:",
            "ldr {a}, [{source}], #8",
            "str {a}, [{destination}], #8",
            "sub {length}, {length}, #8",
            "cmp {length}, #8",
            "b.hs 4b",
            "5:",
            "cbz {length}, 7f",
            "6:",
            "ldrb {a:w}, [{source}], #1",
            "strb {a:w}, [{destination}], #1",
            "subs {length}, {length}, #1",
            "b.ne 6b",
            "7:",
            source = inout(reg) source => _,
            destination = inout(reg) destination => _,
            length = inout(reg) length => _,
            a = out(reg) _,
            b = out(reg) _,
            c = out(reg) _,
            d = out(reg) _,
            options(nostack),
        );
    }
}
