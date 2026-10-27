// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::Error;

/// An owned, physically contiguous allocation visible to a coherent SMMU.
///
/// # Safety
/// Addresses must remain stable until Drop, refer to the same exclusive live
/// allocation, and cover `4096 << order()` bytes with that physical alignment.
/// CPU mappings must be normal cacheable memory in the SMMU's coherency domain.
/// No references into device-written bytes may exist while hardware owns them.
pub unsafe trait DmaMemory {
    fn physical(&self) -> u64;
    fn virtual_address(&self) -> usize;
    fn order(&self) -> usize;
}

/// A retained, page-aligned DMA buffer; the driver never accesses its CPU bytes.
///
/// Unlike control tables, payload memory may be shared with a guest address
/// space. Keeping this owner alive must retain the hardware-write lease as well
/// as the physical pages, even after all originating handles have closed.
///
/// # Safety
/// `length` must be a stable, nonzero multiple of 4096. For every aligned offset
/// below it, `physical_page` must identify a stable live 4096-byte RAM page until
/// Drop. CPU/guest aliases must use compatible coherent attributes and cannot
/// create Rust references racing device writes. Pages must not be substituted
/// or returned to an allocator while any buffer owner remains alive.
pub unsafe trait DmaBuffer {
    fn length(&self) -> u64;
    fn physical_page(&self, offset: u64) -> u64;
}

// SAFETY: DmaMemory owns one stable, aligned contiguous allocation. Its stronger
// exclusive-access contract satisfies the payload lifetime requirements.
unsafe impl<T: DmaMemory> DmaBuffer for T {
    fn length(&self) -> u64 {
        4096u64.checked_shl(self.order() as u32).unwrap_or(0)
    }
    fn physical_page(&self, offset: u64) -> u64 {
        self.physical().wrapping_add(offset)
    }
}

/// Allocation and platform ordering required by the physical driver.
pub trait Environment {
    type Memory: DmaMemory;
    fn allocate(order: usize) -> Result<Self::Memory, Error>;
    /// Full-system completion barrier, including compiler ordering.
    fn synchronize();
    fn now_microseconds() -> u64;
}

pub(super) fn allocate<E: Environment>(order: usize, address_bits: u8) -> Result<E::Memory, Error> {
    let memory = E::allocate(order)?;
    validate(&memory, order, address_bits)?;
    let words = (4096usize << order) / 8;
    for index in 0..words {
        write(&memory, index, 0);
    }
    Ok(memory)
}

pub(super) fn validate<M: DmaMemory>(memory: &M, order: usize, bits: u8) -> Result<(), Error> {
    let size = 4096u64.checked_shl(order as u32).ok_or(Error::Address)?;
    if memory.order() != order
        || !memory.physical().is_multiple_of(size)
        || !memory.virtual_address().is_multiple_of(8)
        || memory
            .physical()
            .checked_add(size)
            .is_none_or(|end| end > (1u64 << bits))
        || memory
            .virtual_address()
            .checked_add(size as usize)
            .is_none()
    {
        return Err(Error::Address);
    }
    Ok(())
}

pub(super) fn read<M: DmaMemory>(memory: &M, word: usize) -> u64 {
    assert!(word < (4096usize << memory.order()) / 8);
    // SAFETY: DmaMemory guarantees a live aligned allocation. Bounds are checked;
    // device-owned storage is accessed without forming a Rust reference.
    unsafe {
        u64::from_le(core::ptr::read_volatile(
            (memory.virtual_address() as *const u64).add(word),
        ))
    }
}

pub(super) fn write<M: DmaMemory>(memory: &M, word: usize, value: u64) {
    assert!(word < (4096usize << memory.order()) / 8);
    // SAFETY: Same owned allocation and bounds as read; callers serialize CPU
    // writers and order descriptor publication before hardware notification.
    unsafe {
        core::ptr::write_volatile(
            (memory.virtual_address() as *mut u64).add(word),
            value.to_le(),
        )
    }
}
