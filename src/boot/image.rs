// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Linker-derived kernel image metadata used during boot.

use core::ptr::addr_of;

use hyper::hal::memory::KernelImageLayout;

unsafe extern "C" {
    static __image_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
    static __image_end: u8;
}

/// Returns the physical image segments exported by the kernel linker script.
pub fn layout() -> KernelImageLayout {
    let start = addr_of!(__image_start) as u64;
    let rodata = addr_of!(__rodata_start) as u64;
    let data = addr_of!(__data_start) as u64;
    let end = addr_of!(__image_end) as u64;
    KernelImageLayout {
        physical_start: start,
        text_size: rodata - start,
        rodata_size: data - rodata,
        total_size: end - start,
    }
}
