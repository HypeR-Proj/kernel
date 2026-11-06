// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![no_std]

mod state;
use core::sync::atomic::Ordering;
include!(concat!(env!("HYPER_VDSO_BUILD"), "/abi.rs"));

unsafe extern "C" {
    static __hyper_vvar: state::State;
}

#[repr(C)]
pub struct Result {
    status: i64,
    value0: u64,
    value1: u64,
}

impl Result {
    const fn unsupported() -> Self {
        Self {
            status: NOT_SUPPORTED,
            value0: 0,
            value1: 0,
        }
    }
    const fn ok(value0: u64, value1: u64) -> Self {
        Self {
            status: 0,
            value0,
            value1,
        }
    }
}

fn data() -> &'static state::State {
    let pointer: *const state::State;
    // SAFETY: The paired loader maps the kernel-owned read-only data page at
    // this image-relative symbol for the complete lifetime of the image. These
    // PC-relative sequences avoid an ELF relocation/GOT and access no memory.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("adrp {p}, {data}", "add {p}, {p}, :lo12:{data}",
            p = out(reg) pointer, data = sym __hyper_vvar, options(nostack, nomem, preserves_flags));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!(".option push", ".option norelax", "lla {p}, {data}", ".option pop",
            p = out(reg) pointer, data = sym __hyper_vvar, options(nostack, nomem));
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("lea {p}, [rip + {data}]",
            p = out(reg) pointer, data = sym __hyper_vvar, options(nostack, nomem, preserves_flags));
        &*pointer
    }
}

fn monotonic() -> Option<u64> {
    #[cfg(target_arch = "aarch64")]
    {
        let frequency = data().frequency.load(Ordering::Acquire);
        if frequency == 0 || frequency > u32::MAX as u64 {
            return None;
        }
        let ticks: u64;
        // SAFETY: A nonzero frequency certifies host EL0 physical-counter
        // access. ISB orders the counter sample; no memory or flags change.
        unsafe {
            core::arch::asm!("isb", "mrs {ticks}, cntpct_el0", ticks = out(reg) ticks,
            options(nostack, preserves_flags));
        }
        (ticks / frequency)
            .checked_mul(1_000_000_000)?
            .checked_add((ticks % frequency) * 1_000_000_000 / frequency)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        None
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn __hyper_vdso_monotonic_v1() -> Result {
    monotonic().map_or_else(Result::unsupported, |ns| Result::ok(ns, 0))
}

#[unsafe(no_mangle)]
pub extern "C" fn __hyper_vdso_realtime_v1() -> Result {
    let Some(now) = monotonic() else {
        return Result::unsupported();
    };
    let state = data();
    let Some(elapsed) = now.checked_sub(state.realtime_monotonic.load(Ordering::Relaxed)) else {
        return Result::unsupported();
    };
    let nanos = state.realtime_nanos.load(Ordering::Relaxed) + elapsed % 1_000_000_000;
    let seconds = state.realtime_seconds.load(Ordering::Relaxed) as i64;
    let Some(seconds) = seconds
        .checked_add((elapsed / 1_000_000_000) as i64)
        .and_then(|s| s.checked_add((nanos / 1_000_000_000) as i64))
    else {
        return Result::unsupported();
    };
    Result::ok(seconds as u64, nanos % 1_000_000_000)
}

#[unsafe(no_mangle)]
pub extern "C" fn __hyper_vdso_ipc_active_v1() -> Result {
    Result::ok(data().ipc_active.load(Ordering::Acquire), 0)
}
