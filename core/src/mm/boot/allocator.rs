// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free early physical-page allocator.

use crate::mm::boot::MAX_MEMORY_REGIONS;
use crate::mm::boot::MAX_RESERVED_REGIONS;
use crate::mm::ranges::PhysicalRange;
use crate::mm::ranges::RegionList;

use crate::mm::{PAGE_SIZE, PhysicalAddress};

pub const MAX_BOOT_RESERVATIONS: usize = MAX_RESERVED_REGIONS + 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootAllocatorError {
    AddressOverflow,
    InvalidAlignment,
    InvalidRequest,
    OutOfMemory,
    TooManyReservations,
    Unaddressable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootMemoryStats {
    pub ram_pages: usize,
    pub reserved_pages: usize,
    pub available_pages: usize,
}

/// Page-granular early physical memory allocator.
///
/// This allocator is intentionally allocation-free. Every returned range is
/// inserted into the reservation set, giving later allocators a complete view
/// of memory consumed during boot.
#[derive(Clone, Copy)]
pub struct BootAllocator {
    memory: RegionList<MAX_MEMORY_REGIONS>,
    reserved: RegionList<MAX_BOOT_RESERVATIONS>,
    accessible_limit: u64,
}

impl BootAllocator {
    pub fn new(
        memory: &RegionList<MAX_MEMORY_REGIONS>,
        firmware_reserved: &RegionList<MAX_RESERVED_REGIONS>,
        accessible_limit: u64,
    ) -> Result<Self, BootAllocatorError> {
        let mut reserved = RegionList::new();
        for &range in firmware_reserved.as_slice() {
            reserved
                .insert(page_covering_range(range)?)
                .map_err(|_| BootAllocatorError::TooManyReservations)?;
        }
        Ok(Self {
            memory: *memory,
            reserved,
            accessible_limit,
        })
    }

    pub fn reserve(&mut self, range: PhysicalRange) -> Result<(), BootAllocatorError> {
        self.reserved
            .insert(page_covering_range(range)?)
            .map_err(|_| BootAllocatorError::TooManyReservations)
    }

    /// Reserves borrowed boot data and records the complete pages that belong
    /// exclusively to this reservation. Partial boundary pages and all earlier
    /// reservations remain permanent. Later boot allocations skip the entire
    /// covering reservation as usual.
    pub fn reserve_temporary(
        &mut self,
        range: PhysicalRange,
    ) -> Result<RegionList<MAX_BOOT_RESERVATIONS>, BootAllocatorError> {
        if !self
            .memory
            .as_slice()
            .iter()
            .any(|memory| memory.start() <= range.start() && range.end() <= memory.end())
        {
            return Err(BootAllocatorError::InvalidRequest);
        }
        let mut owned = RegionList::new();
        let mut cursor = align_up(range.start(), PAGE_SIZE)?;
        let end = range.end() & !(PAGE_SIZE - 1);
        for previous in self.reservations() {
            if previous.end() <= cursor || previous.start() >= end {
                continue;
            }
            if previous.start() > cursor {
                owned
                    .insert(
                        PhysicalRange::new(cursor, previous.start() - cursor)
                            .ok_or(BootAllocatorError::InvalidRequest)?,
                    )
                    .map_err(|_| BootAllocatorError::TooManyReservations)?;
            }
            cursor = previous.end().min(end);
        }
        if cursor < end {
            owned
                .insert(
                    PhysicalRange::new(cursor, end - cursor)
                        .ok_or(BootAllocatorError::InvalidRequest)?,
                )
                .map_err(|_| BootAllocatorError::TooManyReservations)?;
        }
        self.reserve(range)?;
        Ok(owned)
    }

    pub fn allocate_pages(
        &mut self,
        page_count: usize,
        alignment_pages: usize,
    ) -> Result<PhysicalAddress, BootAllocatorError> {
        if page_count == 0 {
            return Err(BootAllocatorError::InvalidRequest);
        }
        if alignment_pages == 0 || !alignment_pages.is_power_of_two() {
            return Err(BootAllocatorError::InvalidAlignment);
        }

        let size = u64::try_from(page_count)
            .ok()
            .and_then(|count| count.checked_mul(PAGE_SIZE))
            .ok_or(BootAllocatorError::AddressOverflow)?;
        let alignment = u64::try_from(alignment_pages)
            .ok()
            .and_then(|count| count.checked_mul(PAGE_SIZE))
            .ok_or(BootAllocatorError::AddressOverflow)?;

        for &region in self.memory.as_slice() {
            let region_end = region.end().min(self.accessible_limit);
            let mut candidate = align_up(region.start(), alignment)?;
            while candidate
                .checked_add(size)
                .filter(|end| *end <= region_end)
                .is_some()
            {
                let allocation = PhysicalRange::new(candidate, size)
                    .ok_or(BootAllocatorError::AddressOverflow)?;
                let overlap_end = self
                    .reserved
                    .as_slice()
                    .iter()
                    .filter(|reserved| reserved.overlaps(allocation))
                    .map(|reserved| reserved.end())
                    .max();
                if let Some(overlap_end) = overlap_end {
                    candidate = align_up(overlap_end, alignment)?;
                    continue;
                }

                self.reserved
                    .insert(allocation)
                    .map_err(|_| BootAllocatorError::TooManyReservations)?;
                return Ok(PhysicalAddress::new(candidate));
            }
        }
        Err(BootAllocatorError::OutOfMemory)
    }

    /// Allocates and zeroes pages through the bootstrap identity map.
    ///
    /// # Safety
    ///
    /// Every allocatable physical address below `accessible_limit` must be
    /// writable through an identity mapping in the current address space.
    pub unsafe fn allocate_zeroed_pages(
        &mut self,
        page_count: usize,
        alignment_pages: usize,
    ) -> Result<PhysicalAddress, BootAllocatorError> {
        let address = self.allocate_pages(page_count, alignment_pages)?;
        let pointer = address
            .as_usize()
            .ok_or(BootAllocatorError::Unaddressable)? as *mut u8;
        let byte_count = page_count
            .checked_mul(PAGE_SIZE as usize)
            .ok_or(BootAllocatorError::AddressOverflow)?;
        // SAFETY: The caller guarantees identity-mapped writable RAM and this
        // allocation is exclusively owned by the boot allocator.
        unsafe { core::ptr::write_bytes(pointer, 0, byte_count) };
        Ok(address)
    }

    pub fn reservations(&self) -> &[PhysicalRange] {
        self.reserved.as_slice()
    }

    pub fn memory(&self) -> &RegionList<MAX_MEMORY_REGIONS> {
        &self.memory
    }

    pub fn handoff(&self) -> super::MemoryHandoff {
        super::MemoryHandoff::new(self.memory, self.reserved)
    }

    /// Returns page counts after intersecting reservations with actual RAM.
    pub fn stats(&self) -> BootMemoryStats {
        let ram_pages = self.memory.as_slice().iter().map(page_aligned_pages).sum();
        let reserved_pages = self
            .memory
            .as_slice()
            .iter()
            .map(|memory| reserved_pages_in(*memory, self.reserved.as_slice()))
            .sum::<usize>()
            .min(ram_pages);
        BootMemoryStats {
            ram_pages,
            reserved_pages,
            available_pages: ram_pages - reserved_pages,
        }
    }
}

fn page_aligned_pages(range: &PhysicalRange) -> usize {
    let Some(start) = range.start().checked_add(PAGE_SIZE - 1) else {
        return 0;
    };
    let start = start & !(PAGE_SIZE - 1);
    let end = range.end() & !(PAGE_SIZE - 1);
    usize::try_from(end.saturating_sub(start) / PAGE_SIZE).unwrap_or(usize::MAX)
}

fn reserved_pages_in(memory: PhysicalRange, reserved: &[PhysicalRange]) -> usize {
    let memory_start = memory.start().saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let memory_end = memory.end() & !(PAGE_SIZE - 1);
    reserved
        .iter()
        .map(|range| {
            let start = range.start().max(memory_start);
            let end = range.end().min(memory_end);
            usize::try_from(end.saturating_sub(start) / PAGE_SIZE).unwrap_or(usize::MAX)
        })
        .sum()
}

fn page_covering_range(range: PhysicalRange) -> Result<PhysicalRange, BootAllocatorError> {
    let start = range.start() & !(PAGE_SIZE - 1);
    let end = align_up(range.end(), PAGE_SIZE)?;
    PhysicalRange::new(start, end - start).ok_or(BootAllocatorError::InvalidRequest)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, BootAllocatorError> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
        .ok_or(BootAllocatorError::AddressOverflow)
}
