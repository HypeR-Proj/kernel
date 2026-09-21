// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::ptr::{read_volatile, write_bytes, write_volatile};

use hyper::hal::memory::KernelImageLayout;
#[cfg(CONFIG_CRASH_CONSOLE)]
use hyper::hal::memory::{Stage1Mapping, Stage1MemoryType};
use hyper::mm::{BootAllocator, BootAllocatorError, PAGE_SIZE, PhysicalAddress, VirtualAddress};
use hyper::platform::{PhysicalRange, PlatformInfo};

use super::address_space::StackMapping;
use super::layout::{
    KERNEL_BASE, KERNEL_STACK_ARENA_BASE, KERNEL_STACK_BASE, LINEAR_BASE, MMIO_BASE,
};
use crate::arch::riscv64::registers;

const LEVEL_SHIFTS: [u64; 3] = [30, 21, 12];
const LEVEL_SIZES: [u64; 3] = [1 << 30, 1 << 21, PAGE_SIZE];
const TABLE_ENTRIES: usize = 512;
// Retained through allocation-free firmware rescanning and kernel boot.
const KERNEL_STACK_PAGES: usize = 64;
const STACK_SLOT_PAGES: usize = 65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    Allocation(BootAllocatorError),
    Conflict,
    InvalidAddress,
    InvalidRange,
    RuntimeAllocation,
    RemoteFence(super::super::sbi::Error),
}

impl From<BootAllocatorError> for Error {
    fn from(error: BootAllocatorError) -> Self {
        Self::Allocation(error)
    }
}

#[derive(Clone, Copy)]
struct Flags(u64);

impl Flags {
    const RW: Self = Self(registers::PTE_READ | registers::PTE_WRITE);
    const RWX: Self = Self(registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE);
    const RO: Self = Self(registers::PTE_READ);
    const RX: Self = Self(registers::PTE_READ | registers::PTE_EXECUTE);

    const fn pte(self) -> u64 {
        self.0 | registers::PTE_VALID | registers::PTE_ACCESSED | registers::PTE_DIRTY
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct FinalAddressSpace {
    pub root: PhysicalAddress,
    pub stack_top: VirtualAddress,
    pub kernel_base: u64,
}

struct Builder<'a> {
    allocator: &'a mut BootAllocator,
    root: PhysicalAddress,
}

impl<'a> Builder<'a> {
    unsafe fn new(allocator: &'a mut BootAllocator) -> Result<Self, Error> {
        // SAFETY: The caller guarantees allocator results are directly writable RAM.
        let root = unsafe { allocator.allocate_zeroed_pages(1, 1)? };
        Ok(Self { allocator, root })
    }

    unsafe fn map_range(
        &mut self,
        virtual_start: u64,
        range: PhysicalRange,
        flags: Flags,
    ) -> Result<(), Error> {
        if virtual_start & (PAGE_SIZE - 1) != range.start() & (PAGE_SIZE - 1) {
            return Err(Error::InvalidRange);
        }
        let physical_start = align_down(range.start());
        let virtual_start = virtual_start
            .checked_sub(range.start() - physical_start)
            .ok_or(Error::AddressOverflow)?;
        let end = align_up(range.end())?;
        let mut offset = 0;
        while physical_start + offset < end {
            let virtual_address = virtual_start + offset;
            let physical_address = physical_start + offset;
            let remaining = end - physical_address;
            let level = best_level(virtual_address, physical_address, remaining);
            // SAFETY: The enclosing mapping contract covers the range and table ownership.
            unsafe { self.map_leaf(virtual_address, physical_address, level, flags)? };
            offset += LEVEL_SIZES[level];
        }
        Ok(())
    }

    unsafe fn map_range_excluding(
        &mut self,
        virtual_offset: u64,
        range: PhysicalRange,
        excluded: &[PhysicalRange],
        flags: Flags,
    ) -> Result<(), Error> {
        let mut cursor = align_down(range.start());
        let end = align_up(range.end())?;
        for reserved in excluded {
            let reserved_start = align_down(reserved.start()).max(cursor);
            let reserved_end = align_up(reserved.end())?.min(end);
            if reserved_start >= reserved_end {
                continue;
            }
            if cursor < reserved_start {
                let segment = PhysicalRange::new(cursor, reserved_start - cursor)
                    .ok_or(Error::InvalidRange)?;
                // SAFETY: This segment excludes reserved ranges and inherits the contract.
                unsafe {
                    self.map_range(
                        virtual_offset
                            .checked_add(cursor)
                            .ok_or(Error::AddressOverflow)?,
                        segment,
                        flags,
                    )?;
                }
            }
            cursor = cursor.max(reserved_end);
        }
        if cursor < end {
            let segment = PhysicalRange::new(cursor, end - cursor).ok_or(Error::InvalidRange)?;
            // SAFETY: This tail excludes reserved ranges and inherits the contract.
            unsafe {
                self.map_range(
                    virtual_offset
                        .checked_add(cursor)
                        .ok_or(Error::AddressOverflow)?,
                    segment,
                    flags,
                )?;
            }
        }
        Ok(())
    }

    unsafe fn map_leaf(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        leaf_level: usize,
        flags: Flags,
    ) -> Result<(), Error> {
        if !canonical(virtual_address) || physical_address >= registers::PHYSICAL_ADDRESS_LIMIT {
            return Err(Error::InvalidAddress);
        }
        let mut table = self.root;
        for level in 0..leaf_level {
            let slot = index(virtual_address, level);
            // SAFETY: Builder owns a live directly accessible hierarchy.
            let entry = unsafe { early_read(table, slot)? };
            table = if entry == 0 {
                // SAFETY: Builder exclusively owns the directly writable allocator.
                let child = unsafe { self.allocator.allocate_zeroed_pages(1, 1)? };
                // SAFETY: The parent table is live, owned, and `slot` is in range.
                unsafe { early_write(table, slot, table_pte(child.get()))? };
                child
            } else if entry & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE)
                == 0
                && entry & registers::PTE_VALID != 0
            {
                PhysicalAddress::new(pte_address(entry))
            } else {
                return Err(Error::Conflict);
            };
        }
        let slot = index(virtual_address, leaf_level);
        let value = leaf_pte(physical_address, flags);
        // SAFETY: Builder owns the live leaf table and `slot` is in range.
        let existing = unsafe { early_read(table, slot)? };
        if existing != 0 && existing != value {
            return Err(Error::Conflict);
        }
        // SAFETY: Builder exclusively owns the live leaf table.
        unsafe { early_write(table, slot, value) }
    }
}

pub(super) unsafe fn build_final_address_space(
    allocator: &mut BootAllocator,
    platform: &PlatformInfo,
    image: KernelImageLayout,
    kernel_base: u64,
) -> Result<FinalAddressSpace, Error> {
    if kernel_base != KERNEL_BASE {
        return Err(Error::InvalidRange);
    }
    // SAFETY: The function contract guarantees directly writable allocator pages.
    let stack = unsafe { allocator.allocate_zeroed_pages(KERNEL_STACK_PAGES, 1)? };
    // SAFETY: The same allocator contract is forwarded to Builder.
    let mut builder = unsafe { Builder::new(allocator)? };
    for &range in platform.memory.as_slice() {
        // SAFETY: Platform ranges are validated and Builder owns all table mutation.
        unsafe {
            // The activation trampoline continues executing at its physical
            // address for a handful of instructions after SATP is written.
            // This executable identity alias is temporary and is retired once
            // every CPU has entered the permanent high mapping.  The linear
            // RAM alias remains non-executable.
            builder.map_range_excluding(0, range, platform.no_map.as_slice(), Flags::RWX)?;
            builder.map_range_excluding(
                LINEAR_BASE,
                range,
                platform.no_map.as_slice(),
                Flags::RW,
            )?;
        }
    }
    for &range in platform.mmio.as_slice() {
        // SAFETY: Platform MMIO ranges are validated and Builder owns table mutation.
        unsafe {
            builder.map_range(range.start(), range, Flags::RW)?;
            builder.map_range(MMIO_BASE + range.start(), range, Flags::RW)?;
        }
    }
    // SAFETY: The image layout is validated and Builder owns table mutation.
    unsafe { map_kernel(&mut builder, image, kernel_base)? };
    // Native Sv39 roots share these upper-level entries. Reserve every root
    // slot in the declared mutable stack arena now; later stack construction
    // changes only shared descendants, never a copied root entry.
    let arena_end = KERNEL_STACK_ARENA_BASE
        .checked_add(hyper::config::MAX_KERNEL_STACKS as u64 * STACK_SLOT_PAGES as u64 * PAGE_SIZE)
        .ok_or(Error::AddressOverflow)?;
    let mut address = KERNEL_STACK_ARENA_BASE;
    while address < arena_end {
        let slot = index(address, 0);
        // SAFETY: Boot exclusively owns this directly accessible root.
        if unsafe { early_read(builder.root, slot)? } == 0 {
            // SAFETY: The boot allocator transfers one zeroed, retained table.
            let child = unsafe { builder.allocator.allocate_zeroed_pages(1, 1)? };
            // SAFETY: The root slot is in range and still unpublished.
            unsafe { early_write(builder.root, slot, table_pte(child.get()))? };
        }
        address = (address | (LEVEL_SIZES[0] - 1))
            .checked_add(1)
            .ok_or(Error::AddressOverflow)?;
    }
    let stack_size = KERNEL_STACK_PAGES as u64 * PAGE_SIZE;
    // SAFETY: `stack` is the fresh allocation above and Builder owns mutation.
    unsafe {
        builder.map_range(
            KERNEL_STACK_BASE + PAGE_SIZE,
            PhysicalRange::new(stack.get(), stack_size).ok_or(Error::InvalidRange)?,
            Flags::RW,
        )?;
    }
    Ok(FinalAddressSpace {
        root: builder.root,
        stack_top: VirtualAddress::new(KERNEL_STACK_BASE + PAGE_SIZE + stack_size),
        kernel_base,
    })
}

unsafe fn map_kernel(
    builder: &mut Builder<'_>,
    image: KernelImageLayout,
    base: u64,
) -> Result<(), Error> {
    let rodata = image.physical_start + image.text_size;
    let data = rodata + image.rodata_size;
    let data_size = image.total_size - image.text_size - image.rodata_size;
    // SAFETY: The caller validates image ranges and grants Builder exclusive mutation.
    unsafe {
        builder.map_range(
            base,
            range(image.physical_start, image.text_size)?,
            Flags::RX,
        )?;
        builder.map_range(
            base + image.text_size,
            range(rodata, image.rodata_size)?,
            Flags::RO,
        )?;
        builder.map_range(
            base + image.text_size + image.rodata_size,
            range(data, data_size)?,
            Flags::RW,
        )
    }
}

pub(super) unsafe fn map_runtime_stack(
    root: PhysicalAddress,
    slot: usize,
    physical: PhysicalAddress,
    pages: usize,
    allocate: &mut dyn FnMut() -> Option<PhysicalAddress>,
) -> Result<StackMapping, Error> {
    let (guard_page, bottom, top) = stack_range(slot, pages)?;
    for page in 0..pages {
        // SAFETY: The caller guarantees allocator pages and serialized mutation.
        unsafe {
            map_runtime_page(
                root,
                bottom as u64 + page as u64 * PAGE_SIZE,
                physical.get() + page as u64 * PAGE_SIZE,
                allocate,
            )?
        };
    }
    flush_tlb()?;
    Ok(StackMapping {
        guard_page,
        bottom,
        top,
    })
}

pub(super) unsafe fn unmap_runtime_stack(
    root: PhysicalAddress,
    slot: usize,
    pages: usize,
) -> Result<(), Error> {
    let (_, bottom, _) = stack_range(slot, pages)?;
    for page in 0..pages {
        let address = bottom as u64 + page as u64 * PAGE_SIZE;
        let (table, slot) = walk_leaf(root, address)?;
        runtime_write(table, slot, 0)?;
    }
    flush_tlb()?;
    Ok(())
}

pub(super) unsafe fn runtime_address_is_mapped(
    root: PhysicalAddress,
    address: u64,
) -> Result<bool, Error> {
    if !canonical(address) {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for level in 0..3 {
        let entry = runtime_read(table, index(address, level))?;
        if entry & registers::PTE_VALID == 0 {
            return Ok(false);
        }
        if entry & (registers::PTE_READ | registers::PTE_EXECUTE) != 0 {
            return Ok(true);
        }
        table = PhysicalAddress::new(pte_address(entry));
    }
    Ok(false)
}

#[cfg(CONFIG_CRASH_CONSOLE)]
pub(super) unsafe fn inspect_runtime_mapping(
    root: PhysicalAddress,
    address: u64,
) -> Result<Option<Stage1Mapping>, Error> {
    if !canonical(address) {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for (level, size) in LEVEL_SIZES.iter().copied().enumerate() {
        let entry = runtime_read(table, index(address, level))?;
        if entry & registers::PTE_VALID == 0 {
            return Ok(None);
        }
        if entry & (registers::PTE_READ | registers::PTE_EXECUTE) != 0 {
            return Ok(Some(Stage1Mapping {
                virtual_start: address & !(size - 1),
                physical_start: pte_address(entry) & !(size - 1),
                size,
                readable: entry & registers::PTE_READ != 0,
                writable: entry & registers::PTE_WRITE != 0,
                executable: entry & registers::PTE_EXECUTE != 0,
                memory_type: Stage1MemoryType::Unknown,
            }));
        }
        table = PhysicalAddress::new(pte_address(entry));
    }
    Ok(None)
}

pub(super) unsafe fn retire_identity_mappings(
    root: PhysicalAddress,
    platform: &PlatformInfo,
) -> Result<(), Error> {
    for &range in platform.memory.as_slice() {
        let mut cursor = align_down(range.start());
        let end = align_up(range.end())?;
        for reserved in platform.no_map.as_slice() {
            let reserved_start = align_down(reserved.start()).max(cursor);
            let reserved_end = align_up(reserved.end())?.min(end);
            if reserved_start >= reserved_end {
                continue;
            }
            retire_identity_range(root, cursor, reserved_start)?;
            cursor = cursor.max(reserved_end);
        }
        retire_identity_range(root, cursor, end)?;
    }
    for &range in platform.mmio.as_slice() {
        retire_identity_range(root, align_down(range.start()), align_up(range.end())?)?;
    }
    flush_tlb()?;
    Ok(())
}

fn retire_identity_range(root: PhysicalAddress, mut address: u64, end: u64) -> Result<(), Error> {
    while address < end {
        let (table, slot, size) = walk_leaf_with_size(root, address)?;
        runtime_write(table, slot, 0)?;
        address = address.checked_add(size).ok_or(Error::AddressOverflow)?;
    }
    Ok(())
}

unsafe fn map_runtime_page(
    root: PhysicalAddress,
    virtual_address: u64,
    physical_address: u64,
    allocate: &mut dyn FnMut() -> Option<PhysicalAddress>,
) -> Result<(), Error> {
    let mut table = root;
    for level in 0..2 {
        let slot = index(virtual_address, level);
        let entry = runtime_read(table, slot)?;
        table = if entry == 0 {
            let child = allocate().ok_or(Error::RuntimeAllocation)?;
            let pointer = runtime_pointer(child)? as *mut u8;
            // SAFETY: The caller exclusively transfers a fresh page-table page.
            unsafe { write_bytes(pointer, 0, PAGE_SIZE as usize) };
            runtime_write(table, slot, table_pte(child.get()))?;
            child
        } else if entry & registers::PTE_VALID != 0
            && entry & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE) == 0
        {
            PhysicalAddress::new(pte_address(entry))
        } else {
            return Err(Error::Conflict);
        };
    }
    let slot = index(virtual_address, 2);
    if runtime_read(table, slot)? != 0 {
        return Err(Error::Conflict);
    }
    runtime_write(table, slot, leaf_pte(physical_address, Flags::RW))
}

fn walk_leaf(root: PhysicalAddress, address: u64) -> Result<(PhysicalAddress, usize), Error> {
    let (table, slot, size) = walk_leaf_with_size(root, address)?;
    if size != PAGE_SIZE {
        return Err(Error::Conflict);
    }
    Ok((table, slot))
}

fn walk_leaf_with_size(
    root: PhysicalAddress,
    address: u64,
) -> Result<(PhysicalAddress, usize, u64), Error> {
    let mut table = root;
    for (level, size) in LEVEL_SIZES.iter().copied().enumerate() {
        let slot = index(address, level);
        let entry = runtime_read(table, slot)?;
        if entry & registers::PTE_VALID == 0 {
            return Err(Error::Conflict);
        }
        if entry & (registers::PTE_READ | registers::PTE_EXECUTE) != 0 {
            return Ok((table, slot, size));
        }
        table = PhysicalAddress::new(pte_address(entry));
    }
    Err(Error::Conflict)
}

fn stack_range(slot: usize, pages: usize) -> Result<(usize, usize, usize), Error> {
    if pages == 0 || pages + 1 > STACK_SLOT_PAGES {
        return Err(Error::InvalidRange);
    }
    let guard = KERNEL_STACK_ARENA_BASE
        .checked_add(slot as u64 * STACK_SLOT_PAGES as u64 * PAGE_SIZE)
        .ok_or(Error::AddressOverflow)?;
    let bottom = guard + PAGE_SIZE;
    let top = bottom + pages as u64 * PAGE_SIZE;
    Ok((guard as usize, bottom as usize, top as usize))
}

fn best_level(virtual_address: u64, physical_address: u64, remaining: u64) -> usize {
    LEVEL_SIZES
        .iter()
        .position(|size| {
            virtual_address.is_multiple_of(*size)
                && physical_address.is_multiple_of(*size)
                && remaining >= *size
        })
        .unwrap_or(2)
}

fn canonical(address: u64) -> bool {
    !((1 << 38)..0xffff_ffc0_0000_0000).contains(&address)
}

fn index(address: u64, level: usize) -> usize {
    ((address >> LEVEL_SHIFTS[level]) & (TABLE_ENTRIES as u64 - 1)) as usize
}

fn table_pte(address: u64) -> u64 {
    (address >> 2) | registers::PTE_VALID
}

fn leaf_pte(address: u64, flags: Flags) -> u64 {
    (address >> 2) | flags.pte()
}

fn pte_address(entry: u64) -> u64 {
    (entry >> registers::PTE_PPN_SHIFT) << registers::PAGE_SHIFT
}

unsafe fn early_read(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = table.as_usize().ok_or(Error::InvalidAddress)? as *const u64;
    // SAFETY: The caller guarantees a directly accessible live table and in-range slot.
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}

unsafe fn early_write(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = table.as_usize().ok_or(Error::InvalidAddress)? as *mut u64;
    // SAFETY: The caller guarantees exclusive access to a live table and in-range slot.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}

fn runtime_read(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = runtime_pointer(table)? as *const u64;
    // SAFETY: Internal callers walk a live hierarchy and produce an in-range slot.
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}

fn runtime_write(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = runtime_pointer(table)? as *mut u64;
    // SAFETY: Internal callers serialize mutation of a live table and in-range slot.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}

fn runtime_pointer(table: PhysicalAddress) -> Result<usize, Error> {
    usize::try_from(LINEAR_BASE + table.get()).map_err(|_| Error::InvalidAddress)
}

fn flush_tlb() -> Result<(), Error> {
    // SAFETY: Publish page-table stores before invalidating this hart. SFENCE.VMA
    // is local, so SBI RFENCE synchronously executes it on every other online
    // hart that can retain translations from this shared address space.
    unsafe { asm!("fence rw, rw", "sfence.vma", options(nostack)) };
    super::super::smp::for_each_online_remote_hart(|hart_id| {
        super::super::sbi::remote_sfence_vma(hart_id, 0, usize::MAX)
    })
    .map_err(Error::RemoteFence)
}

fn range(start: u64, size: u64) -> Result<PhysicalRange, Error> {
    PhysicalRange::new(start, size).ok_or(Error::InvalidRange)
}

fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

fn align_up(value: u64) -> Result<u64, Error> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(align_down)
        .ok_or(Error::AddressOverflow)
}
