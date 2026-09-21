// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::barrier::{Barrier, BarrierAccess, BarrierDomain};
use hyper::hal::cache::{CacheError, CacheMaintenance};
use hyper::sync::atomic::{AtomicU64, Ordering};
use hyper::vm::aarch64::cache::{GuestInstructionCachePolicy, instruction_publication_contract};

use super::barrier::Aarch64Barrier;
use super::registers;

/// `AArch64` cache maintenance using virtual-address operations at EL2.
pub struct Aarch64Cache;

const UNINITIALIZED_CONTRACT: u64 = u64::MAX;
static BOOT_PUBLICATION_CONTRACT: AtomicU64 = AtomicU64::new(UNINITIALIZED_CONTRACT);
static BOOT_CTR_EL0: AtomicU64 = AtomicU64::new(UNINITIALIZED_CONTRACT);

/// Freezes the effective cache-maintenance contract selected by the boot PE.
pub(super) fn prepare_boot_cpu() -> Result<(), CacheError> {
    let ctr_el0 = read_ctr_el0();
    let contract = instruction_publication_contract(ctr_el0);
    let _ = BOOT_CTR_EL0.compare_exchange(
        UNINITIALIZED_CONTRACT,
        ctr_el0,
        Ordering::Release,
        Ordering::Relaxed,
    );
    match BOOT_PUBLICATION_CONTRACT.compare_exchange(
        UNINITIALIZED_CONTRACT,
        contract,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == contract => Ok(()),
        Err(_) => Err(CacheError::InvalidLineSize),
    }
}

/// Checks that this PE can safely consume boot-CPU cache publications.
pub(super) fn current_cpu_is_compatible() -> bool {
    let expected = BOOT_PUBLICATION_CONTRACT.load(Ordering::Acquire);
    expected != UNINITIALIZED_CONTRACT
        && expected == instruction_publication_contract(read_ctr_el0())
}

impl CacheMaintenance for Aarch64Cache {
    fn data_line_size() -> usize {
        selected_data_line_size()
    }

    fn instruction_line_size() -> usize {
        selected_instruction_line_size()
    }

    unsafe fn publish_data_range(start: usize, length: usize) -> Result<(), CacheError> {
        // SAFETY: The trait contract guarantees the rounded range is mapped
        // and remains exclusively owned for this maintenance operation.
        unsafe { maintain_range(start, length, Self::data_line_size(), dc_cvac)? };
        Aarch64Barrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    unsafe fn discard_data_range(start: usize, length: usize) -> Result<(), CacheError> {
        Aarch64Barrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        // SAFETY: The caller owns every rounded line and guarantees that
        // invalidation cannot discard unrelated dirty state.
        unsafe { maintain_range(start, length, Self::data_line_size(), dc_ivac)? };
        Aarch64Barrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    unsafe fn publish_and_discard_data_range(
        start: usize,
        length: usize,
    ) -> Result<(), CacheError> {
        Aarch64Barrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        // SAFETY: Exclusive ownership of the rounded range is required by the
        // trait contract and held through both maintenance phases.
        unsafe { maintain_range(start, length, Self::data_line_size(), dc_civac)? };
        Aarch64Barrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    unsafe fn publish_instruction_range(start: usize, length: usize) -> Result<(), CacheError> {
        if length == 0 {
            return Ok(());
        }
        // SAFETY: The one yielded range has the mapping and ownership promised
        // by the single-range trait contract and remains stable for both
        // possible enumeration passes.
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
        Aarch64Barrier::instruction_synchronization();
    }

    fn synchronize_guest_instruction_migration() {
        // Guest maintenance executes with HCR_EL2.FB, so architecturally local
        // instruction and translation maintenance is promoted to the guest's
        // inner-shareable domain before the vCPU can migrate. No additional
        // destination-PE operation is required here.
    }

    fn invalidate_instruction_all() {
        // SAFETY: IALLUIS invalidates instruction-cache entries in the
        // inner-shareable domain and does not dereference a virtual address.
        unsafe { invalidate_instruction_domain() };
        Aarch64Barrier::data_synchronization(BarrierDomain::InnerShareable, BarrierAccess::All);
        Aarch64Barrier::instruction_synchronization();
    }
}

fn read_ctr_el0() -> u64 {
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "mrs {ctr}, ctr_el0",
            ctr = out(reg) ctr,
            options(nomem, nostack, preserves_flags)
        );
    }
    ctr
}

fn selected_contract() -> u64 {
    let contract = BOOT_PUBLICATION_CONTRACT.load(Ordering::Acquire);
    if contract == UNINITIALIZED_CONTRACT {
        instruction_publication_contract(read_ctr_el0())
    } else {
        contract
    }
}

const fn contract_instruction_line_size(contract: u64) -> usize {
    4usize << (contract & registers::CTR_EL0_LINE_SIZE_MASK)
}

const fn contract_data_line_size(contract: u64) -> usize {
    4usize << ((contract >> 4) & registers::CTR_EL0_LINE_SIZE_MASK)
}

const fn contract_instruction_policy(contract: u64) -> GuestInstructionCachePolicy {
    if contract & (1 << 8) != 0 {
        GuestInstructionCachePolicy::Range
    } else {
        GuestInstructionCachePolicy::WholeInnerShareableDomain
    }
}

fn selected_instruction_line_size() -> usize {
    let ctr_el0 = BOOT_CTR_EL0.load(Ordering::Acquire);
    let ctr_el0 = if ctr_el0 == UNINITIALIZED_CONTRACT {
        read_ctr_el0()
    } else {
        ctr_el0
    };
    4usize << (ctr_el0 & registers::CTR_EL0_LINE_SIZE_MASK)
}

fn selected_data_line_size() -> usize {
    contract_data_line_size(selected_contract())
}

unsafe fn invalidate_instruction_domain() {
    // SAFETY: IALLUIS has no address operand and invalidates instruction-cache
    // entries throughout the inner-shareable domain.
    unsafe { asm!("ic ialluis", options(nostack, preserves_flags)) };
}

unsafe fn maintain_range(
    start: usize,
    length: usize,
    line_size: usize,
    operation: unsafe fn(usize),
) -> Result<(), CacheError> {
    if length == 0 {
        return Ok(());
    }
    let end = start
        .checked_add(length)
        .ok_or(CacheError::AddressOverflow)?;
    let mut line = start & !(line_size - 1);
    while line < end {
        // SAFETY: The caller guarantees each rounded line is mapped and valid
        // for the selected cache operation.
        unsafe { operation(line) };
        line = line
            .checked_add(line_size)
            .ok_or(CacheError::AddressOverflow)?;
    }
    Ok(())
}

unsafe fn dc_cvac(address: usize) {
    // Clean by virtual address to the AArch64 point of coherency.
    // SAFETY: The caller guarantees that this cache line is mapped.
    unsafe { asm!("dc cvac, {address}", address = in(reg) address, options(nostack)) };
}

unsafe fn dc_cvau(address: usize) {
    // Clean by virtual address to the AArch64 point of unification.
    // SAFETY: The caller guarantees that this cache line is mapped.
    unsafe { asm!("dc cvau, {address}", address = in(reg) address, options(nostack)) };
}

unsafe fn dc_ivac(address: usize) {
    // Invalidate by virtual address to the AArch64 point of coherency.
    // SAFETY: The caller guarantees exclusive ownership of this cache line.
    unsafe { asm!("dc ivac, {address}", address = in(reg) address, options(nostack)) };
}

unsafe fn dc_civac(address: usize) {
    // Clean and invalidate by virtual address to the AArch64 point of coherency.
    // SAFETY: The caller guarantees exclusive ownership of this cache line.
    unsafe { asm!("dc civac, {address}", address = in(reg) address, options(nostack)) };
}

unsafe fn ic_ivau(address: usize) {
    // Invalidate by virtual address to the AArch64 point of unification.
    // SAFETY: The caller guarantees that this cache line is mapped.
    unsafe { asm!("ic ivau, {address}", address = in(reg) address, options(nostack)) };
}

/// Cleans data and invalidates instruction state without discarding dirty data.
/// Ranges must be stable, retained coherent mappings; guest writers are allowed.
unsafe fn maintain_instruction_ranges(
    mut ranges: impl FnMut(&mut dyn FnMut(usize, usize)),
) -> Result<(), CacheError> {
    let contract = selected_contract();
    let data_line_size = contract_data_line_size(contract);
    let mut clean_result = Ok(());
    ranges(&mut |start, length| {
        if clean_result.is_ok() {
            // SAFETY: The batch contract keeps every enumerated range
            // mapped and retained throughout both phases.
            clean_result = unsafe { maintain_range(start, length, data_line_size, dc_cvau) };
        }
    });
    // All dirty instruction bytes must reach PoU before any instruction
    // cache invalidation. One barrier covers the complete batch.
    Aarch64Barrier::data_synchronization(BarrierDomain::InnerShareable, BarrierAccess::All);
    clean_result?;

    let invalidate_result = match contract_instruction_policy(contract) {
        GuestInstructionCachePolicy::Range => {
            let instruction_line_size = contract_instruction_line_size(contract);
            let mut result = Ok(());
            ranges(&mut |start, length| {
                if result.is_ok() {
                    // SAFETY: PIPT indexing makes each stable host VA
                    // sufficient to select all copies of that line.
                    result =
                        unsafe { maintain_range(start, length, instruction_line_size, ic_ivau) };
                }
            });
            result
        }
        GuestInstructionCachePolicy::WholeInnerShareableDomain => {
            // SAFETY: Guest execution uses a different VA from the host
            // linear alias. IALLUIS covers every alias in the execution
            // domain without dereferencing a virtual address.
            unsafe { invalidate_instruction_domain() };
            Ok(())
        }
    };
    // Complete every invalidation issued before returning, including when
    // validation of a later range detected an address overflow.
    Aarch64Barrier::data_synchronization(BarrierDomain::InnerShareable, BarrierAccess::All);
    invalidate_result
}
