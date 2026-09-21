// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::cache::{CacheError, CacheMaintenance};
use hyper::sync::atomic::{AtomicUsize, Ordering};

static BLOCK_SIZE: AtomicUsize = AtomicUsize::new(0);

pub struct Riscv64Cache;

pub fn initialize(block_size: usize) -> Result<(), CacheError> {
    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(CacheError::InvalidLineSize);
    }
    BLOCK_SIZE.store(block_size, Ordering::Release);
    Ok(())
}

impl CacheMaintenance for Riscv64Cache {
    fn data_line_size() -> usize {
        BLOCK_SIZE.load(Ordering::Acquire)
    }

    fn instruction_line_size() -> usize {
        BLOCK_SIZE.load(Ordering::Acquire)
    }

    unsafe fn publish_data_range(start: usize, length: usize) -> Result<(), CacheError> {
        // SAFETY: The trait contract guarantees an accessible complete range.
        unsafe { maintain_range(start, length, riscv64_cbo_clean) }
    }

    unsafe fn discard_data_range(start: usize, length: usize) -> Result<(), CacheError> {
        // SAFETY: The trait contract guarantees an accessible complete range.
        unsafe { maintain_range(start, length, riscv64_cbo_inval) }
    }

    unsafe fn publish_and_discard_data_range(
        start: usize,
        length: usize,
    ) -> Result<(), CacheError> {
        // SAFETY: The trait contract guarantees an accessible complete range.
        unsafe { maintain_range(start, length, riscv64_cbo_flush) }
    }

    unsafe fn publish_instruction_range(start: usize, length: usize) -> Result<(), CacheError> {
        if length == 0 {
            return Ok(());
        }
        // SAFETY: The stable one-range enumerator inherits the mapping and
        // ownership guarantees of the single-range contract.
        unsafe { Self::publish_instruction_ranges(|visit| visit(start, length)) }
    }

    unsafe fn publish_instruction_ranges(
        ranges: impl FnMut(&mut dyn FnMut(usize, usize)),
    ) -> Result<(), CacheError> {
        // SAFETY: Host publication provides the stronger exclusive ownership
        // contract required to publish a stable instruction stream.
        unsafe { maintain_instruction_ranges(ranges) }
    }

    unsafe fn prepare_guest_instruction_range(
        start: usize,
        length: usize,
    ) -> Result<(), CacheError> {
        // SAFETY: The guest retains the complete coherent range. This helper
        // cleans data (never discards it), so sibling guest data writes cannot
        // lose dirty contents. Guest software owns concurrent code publication.
        unsafe { maintain_instruction_ranges(|visit| visit(start, length)) }
    }

    fn synchronize_instruction_execution() {
        // SAFETY: FENCE.I has no pointer operands and is valid in HS mode.
        unsafe { asm!("fence.i", options(nostack)) };
    }

    fn synchronize_guest_instruction_migration() {
        // Guest FENCE.I affects only the physical hart which executed it. A
        // migrated virtual hart therefore needs one local FENCE.I before it
        // can consume guest-modified instructions on the destination hart.
        // SAFETY: FENCE.I has no pointer operands and is valid in HS mode.
        unsafe { asm!("fence.i", options(nostack)) };
    }

    fn invalidate_instruction_all() {
        // SAFETY: FENCE.I has no pointer operands and is valid in HS mode.
        unsafe { asm!("fence.i", options(nostack)) };
    }
}

unsafe fn maintain_range(
    start: usize,
    length: usize,
    operation: unsafe extern "C" fn(usize),
) -> Result<(), CacheError> {
    let block_size = initialized_block_size()?;
    // CBOs are ordered as writes or device outputs. Full fences also order
    // ordinary memory and MMIO around ownership transfers to other agents.
    // SAFETY: FENCE has no pointer operands and orders accesses before the CBOs.
    unsafe { asm!("fence iorw, iorw", options(nostack)) };
    // SAFETY: The caller guarantees the rounded blocks are accessible and the
    // operation is one of this module's cache-block routines.
    unsafe { maintain_range_unfenced(start, length, block_size, operation)? };
    // SAFETY: FENCE has no pointer operands and orders the completed CBOs.
    unsafe { asm!("fence iorw, iorw", options(nostack)) };
    Ok(())
}

fn initialized_block_size() -> Result<usize, CacheError> {
    let block_size = BLOCK_SIZE.load(Ordering::Acquire);
    if block_size == 0 {
        Err(CacheError::NotInitialized)
    } else {
        Ok(block_size)
    }
}

unsafe fn maintain_range_unfenced(
    start: usize,
    length: usize,
    block_size: usize,
    operation: unsafe extern "C" fn(usize),
) -> Result<(), CacheError> {
    let end = start
        .checked_add(length)
        .ok_or(CacheError::AddressOverflow)?;
    if length == 0 {
        return Ok(());
    }
    let mut address = start & !(block_size - 1);
    let rounded_end = end
        .checked_add(block_size - 1)
        .map(|value| value & !(block_size - 1))
        .ok_or(CacheError::AddressOverflow)?;
    while address < rounded_end {
        // SAFETY: The caller guarantees this rounded block is accessible and
        // `operation` is one of this module's CBO assembly routines.
        unsafe { operation(address) };
        address = address
            .checked_add(block_size)
            .ok_or(CacheError::AddressOverflow)?;
    }
    Ok(())
}

unsafe extern "C" {
    fn riscv64_cbo_clean(address: usize);
    fn riscv64_cbo_flush(address: usize);
    fn riscv64_cbo_inval(address: usize);
}

/// Cleans data and invalidates instruction state without discarding dirty data.
/// Ranges must be stable, retained coherent mappings; guest writers are allowed.
unsafe fn maintain_instruction_ranges(
    mut ranges: impl FnMut(&mut dyn FnMut(usize, usize)),
) -> Result<(), CacheError> {
    let block_size = initialized_block_size()?;
    // CBO.CLEAN operations are ordered as writes. This path publishes only
    // CPU-written instruction memory, so memory predecessor/successor sets
    // suffice; MMIO ordering belongs to the generic data/device helpers.
    // One pair orders the whole batch instead of fencing each guest page.
    // SAFETY: FENCE has no pointer operands and is valid in HS mode.
    unsafe { asm!("fence rw, rw", options(nostack)) };
    let mut result = Ok(());
    ranges(&mut |start, length| {
        if result.is_ok() {
            // SAFETY: The batch contract guarantees every rounded block is
            // accessible and retained through this transaction.
            result =
                unsafe { maintain_range_unfenced(start, length, block_size, riscv64_cbo_clean) };
        }
    });
    // Complete any clean already issued even if a later range overflowed.
    // SAFETY: FENCE has no pointer operands and is valid in HS mode.
    unsafe { asm!("fence rw, rw", options(nostack)) };
    result?;
    // SAFETY: FENCE.I has no pointer operands and is valid in HS mode.
    unsafe { asm!("fence.i", options(nostack)) };
    Ok(())
}
