// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Intrusive physical-page buddy allocator.

use hyper::mm::boot::{MAX_MEMORY_REGIONS, MemoryHandoff};
use hyper::mm::ranges::PhysicalRange;
use hyper::mm::ranges::RegionList;

use hyper::mm::PAGE_SIZE;
use hyper::mm::PhysicalAddress;
use hyper::mm::boot::MAX_BOOT_RESERVATIONS;

pub const MAX_ORDER: usize = 18;
const NONE: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuddyError {
    AddressOverflow,
    InvalidReservation,
    InvalidOrder,
    OutOfMemory,
    Unaddressable,
}

/// A consistent snapshot of the physical page pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuddyStats {
    /// Pages in the runtime pool, including retired boot reservations.
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
/// Each order indexes free blocks by physical address in an intrusive binary
/// digital tree. Two child links live in each free block; no separately
/// allocated metadata is needed. Tree depth is bounded by the number of
/// block-address bits, even when a fragmented VM releases many isolated pages.
/// All methods require external synchronization.
pub struct BuddyAllocator {
    free_roots: [u64; MAX_ORDER + 1],
    free_blocks: [usize; MAX_ORDER + 1],
    memory: RegionList<MAX_MEMORY_REGIONS>,
    reserved: RegionList<MAX_BOOT_RESERVATIONS>,
    direct_map_base: u64,
    managed_pages: usize,
    free_pages: usize,
    peak_allocated_pages: usize,
    allocation_requests: u64,
    allocation_failures: u64,
    deallocations: u64,
}

impl BuddyAllocator {
    /// Admits a no-longer-referenced boot reservation to the runtime pool.
    /// Validation and reservation splitting finish before any free-tree write.
    /// Allocations already in the pool are never accepted a second time.
    ///
    /// # Safety
    ///
    /// The caller exclusively owns every page in `range`, has retired every
    /// reference/DMA access to it, and has excluded overlapping firmware or
    /// other live reservations. The original writable direct map still exists.
    pub unsafe fn reclaim_reservation(
        &mut self,
        range: PhysicalRange,
    ) -> Result<usize, BuddyError> {
        if range.size() == 0
            || (range.start() | range.end()) & (PAGE_SIZE - 1) != 0
            || !self
                .memory
                .as_slice()
                .iter()
                .any(|memory| memory.start() <= range.start() && range.end() <= memory.end())
            || !self
                .reserved
                .as_slice()
                .iter()
                .any(|reserved| reserved.start() <= range.start() && range.end() <= reserved.end())
        {
            return Err(BuddyError::InvalidReservation);
        }
        // Check the complete interval, including platforms whose physical
        // address space is wider than usize, before changing pool ownership.
        self.pointer(range.start())?;
        self.pointer(range.end() - 1)?;
        let pages =
            usize::try_from(range.size() / PAGE_SIZE).map_err(|_| BuddyError::AddressOverflow)?;
        let managed = self
            .managed_pages
            .checked_add(pages)
            .ok_or(BuddyError::AddressOverflow)?;
        let mut reserved = RegionList::new();
        for &old in self.reserved.as_slice() {
            if !old.overlaps(range) {
                reserved
                    .insert(old)
                    .map_err(|_| BuddyError::InvalidReservation)?;
                continue;
            }
            for part in [
                PhysicalRange::new(old.start(), range.start() - old.start()),
                PhysicalRange::new(range.end(), old.end() - range.end()),
            ]
            .into_iter()
            .flatten()
            {
                reserved
                    .insert(part)
                    .map_err(|_| BuddyError::InvalidReservation)?;
            }
        }
        self.reserved = reserved;
        self.managed_pages = managed;
        self.add_interval(range.start(), range.end())?;
        Ok(pages)
    }

    /// Builds free-block indexes from RAM minus every boot reservation.
    ///
    /// # Safety
    ///
    /// `direct_map_base + PA` must provide writable Normal memory for every
    /// page supplied by `handoff`.
    pub unsafe fn from_handoff(
        handoff: &MemoryHandoff,
        direct_map_base: u64,
    ) -> Result<Self, BuddyError> {
        let (memory, reserved) = handoff.into_regions();
        let mut allocator = Self {
            free_roots: [NONE; MAX_ORDER + 1],
            free_blocks: [0; MAX_ORDER + 1],
            memory,
            reserved,
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
            (order..=MAX_ORDER).find(|&candidate| self.free_roots[candidate] != NONE)
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
    /// allocator. Double-free and mismatched-order calls corrupt ownership.
    pub unsafe fn deallocate(
        &mut self,
        address: PhysicalAddress,
        order: usize,
    ) -> Result<(), BuddyError> {
        if order > MAX_ORDER {
            return Err(BuddyError::InvalidOrder);
        }
        self.free_pages += 1usize << order;
        self.deallocations = self.deallocations.saturating_add(1);
        self.insert_free_block(address.get(), order)
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

    /// Resolves one complete page from the allocator's current managed set.
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
                .memory
                .as_slice()
                .iter()
                .any(|range| range.start() <= physical && end <= range.end())
            || self
                .reserved
                .as_slice()
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
            // Admission may bridge a retired reservation to an existing free
            // interval. Merge now rather than stranding large contiguous RAM
            // until both neighboring blocks happen to be allocated and freed.
            self.insert_free_block(start, order)?;
            self.free_pages += 1usize << order;
            start = start
                .checked_add(block_size(order))
                .ok_or(BuddyError::AddressOverflow)?;
        }
        Ok(())
    }

    fn insert_free_block(&mut self, mut block: u64, mut order: usize) -> Result<(), BuddyError> {
        while order < MAX_ORDER {
            let buddy = block ^ block_size(order);
            if !self.remove(order, buddy)? {
                break;
            }
            block = block.min(buddy);
            order += 1;
        }
        self.push(order, block)
    }

    fn push(&mut self, order: usize, address: u64) -> Result<(), BuddyError> {
        let mut parent = None;
        let mut current = self.free_roots[order];
        let mut depth = 0;
        while current != NONE {
            if current == address {
                return Err(BuddyError::InvalidReservation);
            }
            let branch = Self::branch(address, order, depth)?;
            parent = Some((current, branch));
            current = self.children(current)?[branch];
            depth += 1;
        }
        self.write_children(address, [NONE; 2])?;
        self.replace_link(order, parent, address)?;
        self.free_blocks[order] += 1;
        Ok(())
    }

    fn pop(&mut self, order: usize) -> Result<u64, BuddyError> {
        let address = self.free_roots[order];
        debug_assert_ne!(address, NONE);
        self.remove(order, address)?;
        Ok(address)
    }

    fn remove(&mut self, order: usize, target: u64) -> Result<bool, BuddyError> {
        let mut parent = None;
        let mut current = self.free_roots[order];
        let mut depth = 0;
        while current != NONE {
            if current == target {
                let replacement = self.detach_leaf(current)?;
                self.replace_link(order, parent, replacement)?;
                self.free_blocks[order] -= 1;
                return Ok(true);
            }
            let branch = Self::branch(target, order, depth)?;
            parent = Some((current, branch));
            current = self.children(current)?[branch];
            depth += 1;
        }
        Ok(false)
    }

    /// Routing uses one block-number bit per level, low bits first. Comparing
    /// the exact address precedes this operation: a final node can occur after
    /// all key bits have been consumed, but cannot have distinct descendants.
    fn branch(address: u64, order: usize, depth: u32) -> Result<usize, BuddyError> {
        let shift = PAGE_SIZE.trailing_zeros() + order as u32 + depth;
        address
            .checked_shr(shift)
            .map(|key| (key & 1) as usize)
            .ok_or(BuddyError::InvalidReservation)
    }

    /// Replace a removed node with a descendant leaf. Moving an entire child
    /// subtree upward would change its bit positions and break lookup. A leaf
    /// shares the removed node's ancestor prefix and can inherit its children
    /// at their original depths after its old link has been withdrawn.
    fn detach_leaf(&mut self, removed: u64) -> Result<u64, BuddyError> {
        let mut leaf = removed;
        let mut parent = None;
        loop {
            let children = self.children(leaf)?;
            let branch = if children[0] != NONE { 0 } else { 1 };
            let next = children[branch];
            if next == NONE {
                break;
            }
            parent = Some((leaf, branch));
            leaf = next;
        }
        let Some((parent, branch)) = parent else {
            return Ok(NONE);
        };
        self.write_child(parent, branch, NONE)?;
        // Re-read after unlinking: the leaf may have been a direct child.
        self.write_children(leaf, self.children(removed)?)?;
        Ok(leaf)
    }

    fn replace_link(
        &mut self,
        order: usize,
        parent: Option<(u64, usize)>,
        replacement: u64,
    ) -> Result<(), BuddyError> {
        if let Some((parent, branch)) = parent {
            self.write_child(parent, branch, replacement)?;
        } else {
            self.free_roots[order] = replacement;
        }
        Ok(())
    }

    fn children(&self, physical: u64) -> Result<[u64; 2], BuddyError> {
        let pointer = self.pointer(physical)? as *const [u64; 2];
        // SAFETY: from_handoff establishes a writable direct map for every
        // managed page. Only links reachable from an allocator-owned root are
        // read; an arbitrary buddy address is never dereferenced to infer its
        // ownership from potentially caller-controlled bytes.
        Ok(unsafe { pointer.read() })
    }

    fn write_child(&self, physical: u64, branch: usize, child: u64) -> Result<(), BuddyError> {
        let mut children = self.children(physical)?;
        children[branch] = child;
        self.write_children(physical, children)
    }

    fn write_children(&self, physical: u64, children: [u64; 2]) -> Result<(), BuddyError> {
        let pointer = self.pointer(physical)? as *mut [u64; 2];
        // SAFETY: The allocator exclusively owns every free block and the
        // direct-map contract from construction remains valid.
        unsafe { pointer.write(children) };
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
