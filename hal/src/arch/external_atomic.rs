// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-width observations of memory writable by an untrusted lower world.
//!
//! Rust references/atomic intrinsics impose an access model on overlapping
//! accesses that an untrusted process need not obey. These machine loads have
//! no such Rust aliasing assumption. Values are untrusted, including when the
//! peer uses overlapping writes of another width.

/// # Safety
/// `address` retains a naturally aligned, readable four-byte range of coherent
/// Normal RAM for the call. It must not name device memory.
#[inline]
pub(crate) unsafe fn load_u32(address: *const u32) -> u32 {
    let value: u32;
    // SAFETY: the caller owns the validated RAM admission; each backend issues
    // one naturally aligned load. No reference to peer-writable bytes is made.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("ldr {value:w}, [{address}]", value = out(reg) value,
            address = in(reg) address, options(nostack, preserves_flags));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("lwu {value}, 0({address})", value = out(reg) value,
            address = in(reg) address, options(nostack));
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("mov {value:e}, dword ptr [{address}]", value = out(reg) value,
            address = in(reg) address, options(nostack, preserves_flags));
    }
    value
}

/// # Safety
/// `address` retains a naturally aligned, readable eight-byte range of coherent
/// Normal RAM for the call. It must not name device memory.
#[inline]
pub(crate) unsafe fn load_u64(address: *const u64) -> u64 {
    let value: u64;
    // SAFETY: identical to load_u32, with eight-byte alignment and extent.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("ldr {value}, [{address}]", value = out(reg) value,
            address = in(reg) address, options(nostack, preserves_flags));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("ld {value}, 0({address})", value = out(reg) value,
            address = in(reg) address, options(nostack));
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("mov {value}, qword ptr [{address}]", value = out(reg) value,
            address = in(reg) address, options(nostack, preserves_flags));
    }
    value
}
