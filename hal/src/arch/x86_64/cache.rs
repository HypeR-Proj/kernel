// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::x86_64::__cpuid;

use hyper::hal::barrier::Barrier;
use hyper::hal::cache::{CacheError, CacheMaintenance};
use hyper::sync::atomic::{AtomicUsize, Ordering};

static COHERENCY_LINE_SIZE: AtomicUsize = AtomicUsize::new(0);

pub struct X86_64Cache;

pub fn initialize() -> Result<(), CacheError> {
    let basic = __cpuid(0);
    let features = __cpuid(1);
    let mut line_size = if features.edx & (1 << 19) != 0 {
        ((features.ebx >> 8) & 0xff) as usize * 8
    } else {
        0
    };
    let extended = __cpuid(0x8000_0000);
    if line_size == 0 && extended.eax >= 0x8000_0006 {
        line_size = (__cpuid(0x8000_0006).ecx & 0xff) as usize;
    }
    if basic.eax == 0 || line_size == 0 || !line_size.is_power_of_two() {
        return Err(CacheError::InvalidLineSize);
    }
    COHERENCY_LINE_SIZE.store(line_size, Ordering::Release);
    Ok(())
}

impl CacheMaintenance for X86_64Cache {
    fn data_line_size() -> usize {
        COHERENCY_LINE_SIZE.load(Ordering::Acquire)
    }

    fn instruction_line_size() -> usize {
        COHERENCY_LINE_SIZE.load(Ordering::Acquire)
    }

    unsafe fn publish_data_range(_start: usize, _length: usize) -> Result<(), CacheError> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        Ok(())
    }

    unsafe fn discard_data_range(_start: usize, _length: usize) -> Result<(), CacheError> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        Ok(())
    }

    unsafe fn publish_and_discard_data_range(
        _start: usize,
        _length: usize,
    ) -> Result<(), CacheError> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    unsafe fn publish_instruction_range(_start: usize, _length: usize) -> Result<(), CacheError> {
        super::barrier::X86_64Barrier::instruction_synchronization();
        Ok(())
    }

    unsafe fn publish_instruction_ranges(
        mut ranges: impl FnMut(&mut dyn FnMut(usize, usize)),
    ) -> Result<(), CacheError> {
        // Enumerate once so the owner can validate every discontiguous range;
        // coherent x86 caches need no address-directed maintenance here.
        ranges(&mut |_, _| {});
        // One serializing instruction completes publication for the batch.
        super::barrier::X86_64Barrier::instruction_synchronization();
        Ok(())
    }

    unsafe fn prepare_guest_instruction_range(
        _start: usize,
        _length: usize,
    ) -> Result<(), CacheError> {
        // Coherent instruction/data caches need no destructive maintenance.
        super::barrier::X86_64Barrier::instruction_synchronization();
        Ok(())
    }

    fn synchronize_instruction_execution() {
        super::barrier::X86_64Barrier::instruction_synchronization();
    }

    fn synchronize_guest_instruction_migration() {
        // Coherent instruction and data caches make guest-observed stores
        // visible across logical processors; the scheduler's release/acquire
        // execution handoff supplies the memory-ordering edge.
    }

    fn invalidate_instruction_all() {
        super::barrier::X86_64Barrier::instruction_synchronization();
    }
}
