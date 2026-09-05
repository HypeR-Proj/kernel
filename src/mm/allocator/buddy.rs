// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Intrusive physical-page buddy allocator.

use crate::platform::{MAX_MEMORY_REGIONS, PhysicalRange, RegionList};

use crate::mm::boot::MAX_BOOT_RESERVATIONS;
use crate::mm::{PAGE_SIZE, PhysicalAddress};

pub const MAX_ORDER: usize = 18;
const NONE: u64 = u64::MAX;

#[derive(Clone, Copy)]
pub struct MemoryHandoff {
    memory: RegionList<MAX_MEMORY_REGIONS>,
    reserved: RegionList<MAX_BOOT_RESERVATIONS>,
}

impl MemoryHandoff {
    pub(crate) const fn new(
        memory: RegionList<MAX_MEMORY_REGIONS>,
        reserved: RegionList<MAX_BOOT_RESERVATIONS>,
    ) -> Self {
        Self { memory, reserved }
    }

    pub fn memory(&self) -> &[PhysicalRange] {
        self.memory.as_slice()
    }

    pub fn reserved(&self) -> &[PhysicalRange] {
        self.reserved.as_slice()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuddyError {
    AddressOverflow,
    InvalidOrder,
    OutOfMemory,
    Unaddressable,
}

/// A consistent snapshot of the physical page pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuddyStats {
    /// Pages initially handed to the runtime allocator after boot reservations.
    pub managed_pages: usize,
    pub free_pages: usize,
    pub allocated_pages: usize,
    pub peak_allocated_pages: usize,
    pub allocation_requests: u64,
    pub allocation_failures: u64,
    pub deallocations: u64,
    /// Number of free blocks at each buddy order.
    pub free_blocks: [usize; MAX_ORDER + 1],
}

impl BuddyStats {
    pub fn largest_free_order(&self) -> Option<usize> {
        self.free_blocks.iter().rposition(|&blocks| blocks != 0)
    }
}

/// Intrusive physical page allocator using power-of-two buddy blocks.
///
/// Free-list links are stored in the first word of each free block. The
/// allocator therefore needs only a direct-map base and no separately allocated
/// metadata. All methods require external synchronization.
pub struct BuddyAllocator {
    free_lists: [u64; MAX_ORDER + 1],
    free_blocks: [usize; MAX_ORDER + 1],
    handoff: MemoryHandoff,
    direct_map_base: u64,
    managed_pages: usize,
    free_pages: usize,
    peak_allocated_pages: usize,
    allocation_requests: u64,
    allocation_failures: u64,
    deallocations: u64,
}

impl BuddyAllocator {
    /// Builds free lists from RAM minus every boot reservation.
    ///
    /// # Safety
    ///
    /// `direct_map_base + PA` must provide writable Normal memory for every
    /// page supplied by `handoff`.
    pub unsafe fn from_handoff(
        handoff: &MemoryHandoff,
        direct_map_base: u64,
    ) -> Result<Self, BuddyError> {
        let mut allocator = Self {
            free_lists: [NONE; MAX_ORDER + 1],
            free_blocks: [0; MAX_ORDER + 1],
            handoff: *handoff,
            direct_map_base,
            managed_pages: 0,
            free_pages: 0,
            peak_allocated_pages: 0,
            allocation_requests: 0,
            allocation_failures: 0,
            deallocations: 0,
        };

        for &memory in handoff.memory() {
            let mut cursor = align_up(memory.start(), PAGE_SIZE)?;
            let end = align_down(memory.end(), PAGE_SIZE);
            while cursor < end {
                let next_reserved = handoff
                    .reserved()
                    .iter()
                    .filter(|range| range.end() > cursor && range.start() < end)
                    .min_by_key(|range| range.start());
                let free_end = next_reserved.map_or(end, |range| range.start().min(end));
                if cursor < free_end {
                    allocator.add_interval(cursor, free_end)?;
                }
                cursor = match next_reserved {
                    Some(range) => align_up(range.end(), PAGE_SIZE)?,
                    None => end,
                };
            }
        }
        allocator.managed_pages = allocator.free_pages;
        Ok(allocator)
    }

    pub fn allocate(&mut self, order: usize) -> Result<PhysicalAddress, BuddyError> {
        self.allocation_requests = self.allocation_requests.saturating_add(1);
        if order > MAX_ORDER {
            self.allocation_failures = self.allocation_failures.saturating_add(1);
            return Err(BuddyError::InvalidOrder);
        }
        let Some(source_order) =
            (order..=MAX_ORDER).find(|&candidate| self.free_lists[candidate] != NONE)
        else {
            self.allocation_failures = self.allocation_failures.saturating_add(1);
            return Err(BuddyError::OutOfMemory);
        };
        let block = self.pop(source_order)?;

        for split_order in (order..source_order).rev() {
            let buddy = block
                .checked_add(block_size(split_order))
                .ok_or(BuddyError::AddressOverflow)?;
            self.push(split_order, buddy)?;
        }
        self.free_pages -= 1usize << order;
        self.peak_allocated_pages = self
            .peak_allocated_pages
            .max(self.managed_pages - self.free_pages);
        Ok(PhysicalAddress::new(block))
    }

    /// Returns a block previously allocated with the same order.
    ///
    /// # Safety
    ///
    /// `address` and `order` must describe one live allocation owned by this
    /// allocator. Double-free and mismatched-order calls corrupt free lists.
    pub unsafe fn deallocate(
        &mut self,
        address: PhysicalAddress,
        order: usize,
    ) -> Result<(), BuddyError> {
        if order > MAX_ORDER {
            return Err(BuddyError::InvalidOrder);
        }
        let mut block = address.get();
        let mut current_order = order;
        self.free_pages += 1usize << order;
        self.deallocations = self.deallocations.saturating_add(1);

        while current_order < MAX_ORDER {
            let buddy = block ^ block_size(current_order);
            if !self.remove(current_order, buddy)? {
                break;
            }
            block = block.min(buddy);
            current_order += 1;
        }
        self.push(current_order, block)
    }

    pub const fn free_pages(&self) -> usize {
        self.free_pages
    }

    pub const fn stats(&self) -> BuddyStats {
        BuddyStats {
            managed_pages: self.managed_pages,
            free_pages: self.free_pages,
            allocated_pages: self.managed_pages - self.free_pages,
            peak_allocated_pages: self.peak_allocated_pages,
            allocation_requests: self.allocation_requests,
            allocation_failures: self.allocation_failures,
            deallocations: self.deallocations,
            free_blocks: self.free_blocks,
        }
    }

    pub const fn direct_map_base(&self) -> u64 {
        self.direct_map_base
    }

    /// Resolves one complete page from the allocator's original managed set.
    ///
    /// This checks physical topology before intrusive metadata is read. It
    /// does not prove the page's current ownership; allocator-maintained links
    /// must still be minted only for live metadata pages.
    pub(crate) fn managed_page_pointer(&self, physical: u64) -> Result<usize, BuddyError> {
        let end = physical
            .checked_add(PAGE_SIZE)
            .ok_or(BuddyError::Unaddressable)?;
        if physical & (PAGE_SIZE - 1) != 0
            || !self
                .handoff
                .memory()
                .iter()
                .any(|range| range.start() <= physical && end <= range.end())
            || self
                .handoff
                .reserved()
                .iter()
                .any(|range| physical < range.end() && range.start() < end)
        {
            return Err(BuddyError::Unaddressable);
        }
        let pointer = self.pointer(physical)?;
        pointer
            .checked_add(PAGE_SIZE as usize)
            .ok_or(BuddyError::Unaddressable)?;
        Ok(pointer)
    }

    fn add_interval(&mut self, mut start: u64, end: u64) -> Result<(), BuddyError> {
        while start < end {
            // A sub-page tail cannot back a buddy block. Callers supply
            // page-aligned bounds, so this only guards future refactoring
            // against inserting a block that overruns `end`.
            let Some(size_order) = floor_log2((end - start) / PAGE_SIZE) else {
                break;
            };
            let alignment_order = if start == 0 {
                MAX_ORDER
            } else {
                ((start.trailing_zeros() as usize).saturating_sub(12)).min(MAX_ORDER)
            };
            let order = alignment_order.min(size_order.min(MAX_ORDER));
            self.push(order, start)?;
            self.free_pages += 1usize << order;
            start = start
                .checked_add(block_size(order))
                .ok_or(BuddyError::AddressOverflow)?;
        }
        Ok(())
    }

    fn push(&mut self, order: usize, address: u64) -> Result<(), BuddyError> {
        self.write_next(address, self.free_lists[order])?;
        self.free_lists[order] = address;
        self.free_blocks[order] += 1;
        Ok(())
    }

    fn pop(&mut self, order: usize) -> Result<u64, BuddyError> {
        let address = self.free_lists[order];
        debug_assert_ne!(address, NONE);
        self.free_lists[order] = self.read_next(address)?;
        self.free_blocks[order] -= 1;
        Ok(address)
    }

    fn remove(&mut self, order: usize, target: u64) -> Result<bool, BuddyError> {
        let mut previous = NONE;
        let mut current = self.free_lists[order];
        while current != NONE {
            let next = self.read_next(current)?;
            if current == target {
                if previous == NONE {
                    self.free_lists[order] = next;
                } else {
                    self.write_next(previous, next)?;
                }
                self.free_blocks[order] -= 1;
                return Ok(true);
            }
            previous = current;
            current = next;
        }
        Ok(false)
    }

    fn read_next(&self, physical: u64) -> Result<u64, BuddyError> {
        let pointer = self.pointer(physical)? as *const u64;
        // SAFETY: from_handoff establishes a writable direct map for every
        // managed page; free-list nodes always point to initialized metadata
        // in those pages. Allocator locking, not volatile access, orders this
        // ordinary cacheable memory between CPUs.
        Ok(unsafe { pointer.read() })
    }

    fn write_next(&self, physical: u64, next: u64) -> Result<(), BuddyError> {
        let pointer = self.pointer(physical)? as *mut u64;
        // SAFETY: The allocator exclusively owns every free block and the
        // direct-map contract from construction remains valid.
        unsafe { pointer.write(next) };
        Ok(())
    }

    fn pointer(&self, physical: u64) -> Result<usize, BuddyError> {
        self.direct_map_base
            .checked_add(physical)
            .and_then(|address| usize::try_from(address).ok())
            .ok_or(BuddyError::Unaddressable)
    }
}

const fn block_size(order: usize) -> u64 {
    PAGE_SIZE << order
}

/// Returns the largest `n` with `2^n <= value`, or `None` when `value` is zero.
fn floor_log2(value: u64) -> Option<usize> {
    if value == 0 {
        return None;
    }
    Some((u64::BITS - 1 - value.leading_zeros()) as usize)
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, BuddyError> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| align_down(rounded, alignment))
        .ok_or(BuddyError::AddressOverflow)
}
