// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stage-1 page-table ownership, walking, and mutation.
//!
//! This module owns boot-time table allocation and live hierarchy mutation.
//! Architectural descriptor bits live in `descriptor`, while `mapping_plan`
//! decides which validated platform intervals are mapped. Runtime mutations
//! remain serialized by callers and complete with the required EL2 TLBI
//! sequence before their results become observable.

use core::arch::asm;
use core::ptr::{read_volatile, write_bytes, write_volatile};

#[cfg(feature = "crash-console")]
use hyper::hal::memory::Stage1Mapping;
use hyper::mm::ranges::PhysicalRange;
use hyper::mm::{BootAllocator, BootAllocatorError, PAGE_SIZE, PhysicalAddress, VirtualAddress};

use super::address_space::StackMapping;
use super::layout::{self, RootRegion};
use {super::super::registers, crate::arch::aarch64::memory::address};

mod descriptor;
mod mapping_plan;

use descriptor::MappingFlags;
pub(super) use mapping_plan::{FinalAddressSpace, build_final_address_space};

/// CPU0 stack retained through allocation-free firmware rescanning and boot.
pub(super) use crate::arch::aarch64::memory::address_layout::BOOT_STACK_PAGES as KERNEL_STACK_PAGES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    Allocation(BootAllocatorError),
    Conflict,
    InvalidAddress,
    InvalidRange,
    RuntimeAllocation,
}

impl From<BootAllocatorError> for Error {
    fn from(error: BootAllocatorError) -> Self {
        Self::Allocation(error)
    }
}

struct PageTableBuilder<'allocator> {
    allocator: &'allocator mut BootAllocator,
    root: PhysicalAddress,
    region: RootRegion,
}

impl<'allocator> PageTableBuilder<'allocator> {
    /// Creates an empty four-level EL2 stage-1 translation hierarchy.
    ///
    /// # Safety
    ///
    /// The allocator's accessible range must be writable through the current
    /// bootstrap identity map.
    unsafe fn new(
        allocator: &'allocator mut BootAllocator,
        region: RootRegion,
    ) -> Result<Self, Error> {
        // SAFETY: The caller guarantees all allocator output is writable via
        // the active bootstrap identity mapping.
        let root = unsafe { allocator.allocate_zeroed_pages(1, 1)? };
        Ok(Self {
            allocator,
            root,
            region,
        })
    }

    fn root(&self) -> PhysicalAddress {
        self.root
    }

    /// Maps a physical interval, rounding its ends outward to page boundaries.
    ///
    /// # Safety
    ///
    /// The page-table pages allocated by this builder must remain accessible
    /// through the bootstrap identity mapping until activation completes.
    unsafe fn map_range(
        &mut self,
        virtual_start: VirtualAddress,
        physical: PhysicalRange,
        flags: MappingFlags,
    ) -> Result<(), Error> {
        if virtual_start.get() & (PAGE_SIZE - 1) != physical.start() & (PAGE_SIZE - 1) {
            return Err(Error::InvalidRange);
        }
        let physical_start = align_down(physical.start(), PAGE_SIZE);
        let virtual_base = virtual_start
            .get()
            .checked_sub(physical.start() - physical_start)
            .ok_or(Error::AddressOverflow)?;
        let physical_end = align_up(physical.end(), PAGE_SIZE)?;
        let mut offset = 0;
        let length = physical_end - physical_start;

        while offset < length {
            let virtual_address = virtual_base
                .checked_add(offset)
                .ok_or(Error::AddressOverflow)?;
            let physical_address = physical_start
                .checked_add(offset)
                .ok_or(Error::AddressOverflow)?;
            let remaining = length - offset;
            let level =
                descriptor::best_mapping_level(virtual_address, physical_address, remaining);
            // SAFETY: This method's contract keeps every builder-owned table
            // page accessible through the bootstrap identity map.
            unsafe { self.map_leaf(virtual_address, physical_address, level, flags)? };
            offset += registers::STAGE1_LEVEL_SIZES_4K[level];
        }
        Ok(())
    }

    unsafe fn map_leaf(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        leaf_level: usize,
        flags: MappingFlags,
    ) -> Result<(), Error> {
        if !region_contains(self.region, virtual_address)
            || physical_address >= address::physical_address_limit()
            || leaf_level == 0
        {
            return Err(Error::InvalidAddress);
        }

        let mut table = self.root;
        for level in 0..leaf_level {
            let index = descriptor::table_index(virtual_address, level);
            // SAFETY: The builder exclusively owns this identity-mapped table.
            let entry = unsafe { read_entry(table, index)? };
            table = if descriptor::is_table(entry) {
                PhysicalAddress::new(descriptor::output_address(entry))
            } else if entry == 0 {
                // SAFETY: The builder's allocator contract guarantees a new,
                // writable identity-mapped zeroed table page.
                let child = unsafe { self.allocator.allocate_zeroed_pages(1, 1)? };
                // SAFETY: `table` is builder-owned and `index` is within its
                // architecturally fixed 512-entry page.
                unsafe { write_entry(table, index, descriptor::table(child.get()))? };
                child
            } else {
                return Err(Error::Conflict);
            };
        }

        let index = descriptor::table_index(virtual_address, leaf_level);
        let descriptor = descriptor::leaf(physical_address, leaf_level, flags);
        // SAFETY: The leaf table remains exclusively builder-owned and mapped.
        let existing = unsafe { read_entry(table, index)? };
        if existing != 0 && existing != descriptor {
            return Err(Error::Conflict);
        }
        // SAFETY: The validated slot belongs to the same exclusive table page.
        unsafe { write_entry(table, index, descriptor)? };
        Ok(())
    }
}

pub(super) fn map_runtime_stack(
    root: PhysicalAddress,
    slot: usize,
    physical: PhysicalAddress,
    pages: usize,
    allocate_table: &mut dyn FnMut() -> Option<PhysicalAddress>,
) -> Result<StackMapping, Error> {
    let (guard_page, bottom, top) = stack_slot_range(slot, pages)?;
    let mut mapped = 0usize;
    while mapped < pages {
        let virtual_address = bottom
            .checked_add(mapped * PAGE_SIZE as usize)
            .ok_or(Error::AddressOverflow)? as u64;
        let physical_address = physical
            .get()
            .checked_add(mapped as u64 * PAGE_SIZE)
            .ok_or(Error::AddressOverflow)?;
        map_runtime_page(root, virtual_address, physical_address, allocate_table).inspect_err(
            |_| {
                let _ = unmap_runtime_pages(root, bottom as u64, mapped);
                flush_stage1_tlb();
            },
        )?;
        mapped += 1;
    }
    flush_stage1_tlb();
    Ok(StackMapping {
        guard_page,
        bottom,
        top,
    })
}

pub(super) fn unmap_runtime_stack(
    root: PhysicalAddress,
    slot: usize,
    pages: usize,
) -> Result<(), Error> {
    let (_, bottom, _) = stack_slot_range(slot, pages)?;
    unmap_runtime_pages(root, bottom as u64, pages)?;
    flush_stage1_tlb();
    Ok(())
}

pub(super) fn runtime_address_is_mapped(
    root: PhysicalAddress,
    address: u64,
) -> Result<bool, Error> {
    if !layout::HOST_LAYOUT.upper().contains(address) {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for level in 0..4 {
        let entry = read_runtime_entry(table, descriptor::table_index(address, level))?;
        if entry == 0 {
            return Ok(false);
        }
        if descriptor::is_leaf(entry, level) {
            return Ok(true);
        }
        if !descriptor::is_table(entry) {
            return Ok(false);
        }
        table = PhysicalAddress::new(descriptor::output_address(entry));
    }
    Ok(false)
}

#[cfg(feature = "crash-console")]
pub(super) fn inspect_runtime_mapping(
    root: PhysicalAddress,
    address: u64,
) -> Result<Option<Stage1Mapping>, Error> {
    if !layout::HOST_LAYOUT.upper().contains(address) {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for level in 0..registers::STAGE1_LEVEL_SIZES_4K.len() {
        let entry = read_runtime_entry(table, descriptor::table_index(address, level))?;
        if entry == 0 {
            return Ok(None);
        }
        if let Some(mapping) = descriptor::decode_mapping(entry, level, address) {
            return Ok(Some(mapping));
        }
        if !descriptor::is_table(entry) {
            return Ok(None);
        }
        table = PhysicalAddress::new(descriptor::output_address(entry));
    }
    Ok(None)
}

fn stack_slot_range(slot: usize, pages: usize) -> Result<(usize, usize, usize), Error> {
    let (guard, bottom, top) = layout::HOST_LAYOUT
        .stack_slot(
            u64::try_from(slot).map_err(|_| Error::InvalidRange)?,
            u64::try_from(pages).map_err(|_| Error::InvalidRange)?,
        )
        .ok_or(Error::InvalidRange)?;
    Ok((
        usize::try_from(guard).map_err(|_| Error::InvalidAddress)?,
        usize::try_from(bottom).map_err(|_| Error::InvalidAddress)?,
        usize::try_from(top).map_err(|_| Error::InvalidAddress)?,
    ))
}

fn map_runtime_page(
    root: PhysicalAddress,
    virtual_address: u64,
    physical_address: u64,
    allocate_table: &mut dyn FnMut() -> Option<PhysicalAddress>,
) -> Result<(), Error> {
    if !virtual_address.is_multiple_of(PAGE_SIZE)
        || !physical_address.is_multiple_of(PAGE_SIZE)
        || !layout::HOST_LAYOUT
            .stack_arena()
            .contains_span(virtual_address, PAGE_SIZE)
        || physical_address >= address::physical_address_limit()
    {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for level in 0..3 {
        let index = descriptor::table_index(virtual_address, level);
        let entry = read_runtime_entry(table, index)?;
        table = if entry == 0 {
            let child = allocate_table().ok_or(Error::RuntimeAllocation)?;
            zero_runtime_table(child)?;
            write_runtime_entry(table, index, descriptor::table(child.get()))?;
            child
        } else if descriptor::is_table(entry) {
            PhysicalAddress::new(descriptor::output_address(entry))
        } else {
            return Err(Error::Conflict);
        };
    }
    let index = descriptor::table_index(virtual_address, 3);
    if read_runtime_entry(table, index)? != 0 {
        return Err(Error::Conflict);
    }
    let descriptor = descriptor::leaf(physical_address, 3, MappingFlags::NORMAL_RW);
    write_runtime_entry(table, index, descriptor)
}

fn unmap_runtime_pages(
    root: PhysicalAddress,
    virtual_start: u64,
    pages: usize,
) -> Result<(), Error> {
    for page in 0..pages {
        let address = virtual_start
            .checked_add(page as u64 * PAGE_SIZE)
            .ok_or(Error::AddressOverflow)?;
        let mut table = root;
        for level in 0..3 {
            let entry = read_runtime_entry(table, descriptor::table_index(address, level))?;
            if !descriptor::is_table(entry) {
                return Err(Error::Conflict);
            }
            table = PhysicalAddress::new(descriptor::output_address(entry));
        }
        let index = descriptor::table_index(address, 3);
        if read_runtime_entry(table, index)? == 0 {
            return Err(Error::Conflict);
        }
        write_runtime_entry(table, index, 0)?;
    }
    Ok(())
}

fn zero_runtime_table(table: PhysicalAddress) -> Result<(), Error> {
    let address = core::ptr::with_exposed_provenance_mut::<u8>(runtime_table_address(table)?);
    // SAFETY: The newly allocated page-table page is exclusively owned and
    // writable through the permanent linear map.
    unsafe { write_bytes(address, 0, PAGE_SIZE as usize) };
    Ok(())
}

fn flush_stage1_tlb() {
    // SAFETY: Host hierarchy mutations affect globally visible EL2 stage-1 entries.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi alle2is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}

/// Disconnects the dedicated lower transition hierarchy after every CPU has
/// moved execution, data, devices, and its stack to permanent upper aliases.
///
/// Boot allocation owns all table pages for the kernel lifetime. Clearing the
/// root retires translations, but does not return its detached child tables to
/// the allocator. The now-empty root remains the idle TTBR0 hierarchy.
pub(super) fn retire_identity_mappings(root: PhysicalAddress) -> Result<(), Error> {
    let entries = core::ptr::with_exposed_provenance_mut::<u64>(runtime_table_address(root)?);
    for index in 0..PAGE_SIZE as usize / core::mem::size_of::<u64>() {
        // SAFETY: The caller serializes hierarchy changes and all participating
        // CPUs use upper aliases. Each aligned store invalidates one descriptor
        // in the exclusively owned, permanently mapped transition root.
        unsafe { write_volatile(entries.add(index), 0) };
    }

    // Complete invalidation on every participating CPU before the disconnected
    // lower hierarchy is considered retired.
    flush_stage1_tlb();
    Ok(())
}

fn read_runtime_entry(table: PhysicalAddress, index: usize) -> Result<u64, Error> {
    let base = core::ptr::with_exposed_provenance::<u64>(runtime_table_address(table)?);
    // SAFETY: Final page-table pages remain mapped through the RAM linear map.
    Ok(unsafe { read_volatile(base.add(index)) })
}

fn write_runtime_entry(table: PhysicalAddress, index: usize, value: u64) -> Result<(), Error> {
    let base = core::ptr::with_exposed_provenance_mut::<u64>(runtime_table_address(table)?);
    // SAFETY: The active architecture owns these page tables exclusively.
    unsafe { write_volatile(base.add(index), value) };
    Ok(())
}

fn runtime_table_address(table: PhysicalAddress) -> Result<usize, Error> {
    super::linear_page_address(table).ok_or(Error::InvalidAddress)
}

const fn region_contains(region: RootRegion, address: u64) -> bool {
    match region {
        RootRegion::Lower => layout::HOST_LAYOUT.lower().contains(address),
        RootRegion::Upper => layout::HOST_LAYOUT.upper().contains(address),
    }
}

unsafe fn read_entry(table: PhysicalAddress, index: usize) -> Result<u64, Error> {
    let address = table.as_usize().ok_or(Error::InvalidAddress)?;
    let base = core::ptr::with_exposed_provenance::<u64>(address);
    // SAFETY: The builder owns a zeroed, identity-mapped table page.
    Ok(unsafe { read_volatile(base.add(index)) })
}

unsafe fn write_entry(table: PhysicalAddress, index: usize, value: u64) -> Result<(), Error> {
    let address = table.as_usize().ok_or(Error::InvalidAddress)?;
    let base = core::ptr::with_exposed_provenance_mut::<u64>(address);
    // SAFETY: The builder owns a writable, identity-mapped table page.
    unsafe { write_volatile(base.add(index), value) };
    Ok(())
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Result<u64, Error> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| align_down(rounded, alignment))
        .ok_or(Error::AddressOverflow)
}
