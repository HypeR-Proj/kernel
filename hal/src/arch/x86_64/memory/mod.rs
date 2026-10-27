// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod kaslr;
pub(crate) mod tlb;

use core::ptr::{read_volatile, write_bytes, write_volatile};

use hyper::hal::memory::{AddressTranslation, KernelImageLayout, VirtualMemoryLayout};
#[cfg(feature = "crash-console")]
use hyper::hal::memory::{Stage1Mapping, Stage1MemoryType};
use hyper::mm::{BootAllocator, BootAllocatorError, PAGE_SIZE, PhysicalAddress, VirtualAddress};
use hyper::sync::PublishedOnce;
use {hyper::mm::ranges::PhysicalRange, hyper_platform::PlatformInfo};

use super::registers;

const LEVEL_SHIFTS: [u64; 4] = [39, 30, 21, 12];
const LEVEL_SIZES: [u64; 4] = [1 << 39, 1 << 30, 1 << 21, PAGE_SIZE];
const ENTRIES: usize = 512;
// Retained through allocation-free firmware discovery and kernel boot.
const STACK_PAGES: usize = 64;
const _: () = assert!(STACK_PAGES as u64 * PAGE_SIZE >= 256 * 1024);
const STACK_SLOT_PAGES: usize = 65;
const REGION_SIZE: u64 = 1 << 40;
const STACK_ARENA_BASE: u64 = registers::PML4_STACK_BASE + 2 * 1024 * 1024;
#[derive(Clone, Copy)]
struct KernelImageMapping {
    virtual_start: u64,
    physical_start: u64,
    size: u64,
}

static KERNEL_IMAGE_MAPPING: PublishedOnce<KernelImageMapping> = PublishedOnce::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    AlreadyPrepared,
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

pub struct X86_64AddressTranslation;

impl AddressTranslation for X86_64AddressTranslation {
    fn bootstrap_accessible_limit() -> u64 {
        1 << 30
    }

    fn layout() -> VirtualMemoryLayout {
        VirtualMemoryLayout {
            mmio_base: registers::PML4_MMIO_BASE,
            linear_base: registers::PML4_LINEAR_BASE,
            kernel_base: registers::PML4_KERNEL_BASE,
        }
    }

    fn linear_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        translate(registers::PML4_LINEAR_BASE, physical)
    }

    fn mmio_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
        translate(registers::PML4_MMIO_BASE, physical)
    }
}

fn translate(base: u64, physical: PhysicalAddress) -> Option<VirtualAddress> {
    (physical.get() < REGION_SIZE)
        .then(|| base.checked_add(physical.get()))
        .flatten()
        .map(VirtualAddress::new)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackMapping {
    pub guard_page: usize,
    pub bottom: usize,
    pub top: usize,
}

pub struct PreparedAddressSpace {
    root: PhysicalAddress,
    stack_top: VirtualAddress,
    kernel_base: u64,
}

#[derive(Clone, Copy)]
pub struct ActivationContext {
    pub(super) root: PhysicalAddress,
    pub(super) stack_top: VirtualAddress,
    pub(super) kernel_base: u64,
}

/// Inert permanent-translation state copied into a secondary boot handoff.
#[derive(Clone, Copy)]
pub struct SecondaryActivationContext {
    pub(super) root: PhysicalAddress,
}

impl PreparedAddressSpace {
    pub fn root_address(&self) -> u64 {
        self.root.get()
    }

    pub fn kernel_base(&self) -> u64 {
        self.kernel_base
    }

    pub fn activation_context(&self) -> ActivationContext {
        ActivationContext {
            root: self.root,
            stack_top: self.stack_top,
            kernel_base: self.kernel_base,
        }
    }

    pub fn secondary_activation_context(&self) -> SecondaryActivationContext {
        SecondaryActivationContext { root: self.root }
    }

    /// Removes bootstrap identity mappings.
    ///
    /// # Safety
    ///
    /// The caller must serialize page-table mutation and ensure no CPU can
    /// concurrently walk or use the mappings being retired.
    pub unsafe fn retire_identity_mappings(&self, _platform: &PlatformInfo) -> Result<(), Error> {
        runtime_write(self.root, 0, 0)?;
        crate::arch::x86_64::memory::tlb::flush_all_online();
        Ok(())
    }

    /// Maps a per-CPU kernel stack.
    ///
    /// # Safety
    ///
    /// Every page returned by `allocate` must be uniquely owned, zeroed,
    /// page-aligned RAM that remains live and linearly accessible while the
    /// address space uses it. The caller must serialize page-table mutation.
    pub unsafe fn map_stack(
        &self,
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
                    self.root,
                    bottom as u64 + page as u64 * PAGE_SIZE,
                    physical.get() + page as u64 * PAGE_SIZE,
                    allocate,
                )?
            };
        }
        crate::arch::x86_64::memory::tlb::flush_all_online();
        Ok(StackMapping {
            guard_page,
            bottom,
            top,
        })
    }

    /// Unmaps a per-CPU kernel stack.
    ///
    /// # Safety
    ///
    /// Page-table mutation must be serialized, and no CPU may retain or use a
    /// translation for the stack while it is being removed.
    pub unsafe fn unmap_stack(&self, slot: usize, pages: usize) -> Result<(), Error> {
        let (_, bottom, _) = stack_range(slot, pages)?;
        for page in 0..pages {
            let (table, index) = walk_leaf(self.root, bottom as u64 + page as u64 * PAGE_SIZE)?;
            runtime_write(table, index, 0)?;
        }
        crate::arch::x86_64::memory::tlb::flush_all_online();
        Ok(())
    }

    /// Reads the live page-table hierarchy.
    ///
    /// # Safety
    ///
    /// The caller must prevent concurrent mutation of the hierarchy.
    pub unsafe fn address_is_mapped(&self, address: usize) -> Result<bool, Error> {
        mapping(self.root, address as u64).map(|value| value.is_some())
    }
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
        flags: u64,
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
            let level = best_level(virtual_address, physical_address, end - physical_address);
            // SAFETY: The enclosing mapping contract covers range and table ownership.
            unsafe { self.map_leaf(virtual_address, physical_address, level, flags)? };
            offset += LEVEL_SIZES[level];
        }
        Ok(())
    }

    unsafe fn map_leaf(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        leaf_level: usize,
        flags: u64,
    ) -> Result<(), Error> {
        if !canonical(virtual_address) || physical_address >= registers::PHYSICAL_ADDRESS_LIMIT {
            return Err(Error::InvalidAddress);
        }
        let mut table = self.root;
        for level in 0..leaf_level {
            let index = index(virtual_address, level);
            // SAFETY: Builder owns a live directly accessible hierarchy.
            let entry = unsafe { early_read(table, index)? };
            table = if entry == 0 {
                // SAFETY: Builder exclusively owns the directly writable allocator.
                let child = unsafe { self.allocator.allocate_zeroed_pages(1, 1)? };
                // SAFETY: The parent table is live, owned, and index is in range.
                unsafe {
                    early_write(
                        table,
                        index,
                        child.get() | registers::PAGE_PRESENT | registers::PAGE_WRITE,
                    )?
                };
                child
            } else if entry & registers::PAGE_PRESENT != 0 && entry & registers::PAGE_LARGE == 0 {
                PhysicalAddress::new(pte_address(entry))
            } else {
                return Err(Error::Conflict);
            };
        }
        let index = index(virtual_address, leaf_level);
        let large = (leaf_level < 3) as u64 * registers::PAGE_LARGE;
        let value = physical_address | flags | large | registers::PAGE_PRESENT;
        // SAFETY: Builder owns the live leaf table and index is in range.
        let existing = unsafe { early_read(table, index)? };
        if existing != 0 && existing != value {
            return Err(Error::Conflict);
        }
        // SAFETY: Builder exclusively owns the live leaf table.
        unsafe { early_write(table, index, value) }
    }
}

pub unsafe fn prepare(
    allocator: &mut BootAllocator,
    platform: &PlatformInfo,
    image: KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, Error> {
    if kernel_base != registers::PML4_KERNEL_BASE {
        return Err(Error::InvalidRange);
    }
    // SAFETY: The function contract guarantees directly writable allocator pages.
    let stack = unsafe { allocator.allocate_zeroed_pages(STACK_PAGES, 1)? };
    // SAFETY: The same allocator contract is forwarded to Builder.
    let mut builder = unsafe { Builder::new(allocator)? };
    let rw_nx = registers::PAGE_WRITE | registers::PAGE_NO_EXECUTE;
    for &range in platform.memory.as_slice() {
        // SAFETY: Platform RAM ranges are validated and Builder owns mutation.
        unsafe {
            builder.map_range(0, range, registers::PAGE_WRITE)?;
            builder.map_range(registers::PML4_LINEAR_BASE, range, rw_nx)?;
        }
    }
    let device_flags = rw_nx | registers::PAGE_CACHE_DISABLE | registers::PAGE_WRITE_THROUGH;
    for &range in platform.mmio.as_slice() {
        // SAFETY: Platform MMIO ranges are validated and Builder owns mutation.
        unsafe {
            builder.map_range(
                registers::PML4_MMIO_BASE + range.start(),
                range,
                device_flags,
            )?;
        }
    }
    // SAFETY: Image layout is validated and Builder owns table mutation.
    unsafe { map_kernel(&mut builder, image, kernel_base)? };
    // SAFETY: `stack` is the fresh allocation above and Builder owns mutation.
    unsafe {
        builder.map_range(
            registers::PML4_STACK_BASE + PAGE_SIZE,
            PhysicalRange::new(stack.get(), STACK_PAGES as u64 * PAGE_SIZE)
                .ok_or(Error::InvalidRange)?,
            rw_nx,
        )?;
    }
    let prepared = PreparedAddressSpace {
        root: builder.root,
        stack_top: VirtualAddress::new(
            registers::PML4_STACK_BASE + PAGE_SIZE + STACK_PAGES as u64 * PAGE_SIZE,
        ),
        kernel_base,
    };
    KERNEL_IMAGE_MAPPING
        .publish(KernelImageMapping {
            virtual_start: kernel_base,
            physical_start: image.physical_start,
            size: image.total_size,
        })
        .map_err(|_| Error::AlreadyPrepared)?;
    Ok(prepared)
}

/// Translates an address within the permanently mapped kernel image.
///
/// VMX and SVM control structures are statically allocated in this image and
/// require physical addresses. Keeping the immutable translation fact in the
/// architecture memory mechanism avoids consulting kernel boot policy from a
/// runtime VM-entry path.
pub(super) fn kernel_image_physical_address(virtual_address: usize) -> Option<u64> {
    let mapping = KERNEL_IMAGE_MAPPING.get()?;
    let offset = (virtual_address as u64).checked_sub(mapping.virtual_start)?;
    (offset < mapping.size)
        .then(|| mapping.physical_start.checked_add(offset))
        .flatten()
}

unsafe fn map_kernel(
    builder: &mut Builder<'_>,
    image: KernelImageLayout,
    base: u64,
) -> Result<(), Error> {
    let rodata = image.physical_start + image.text_size;
    let data = rodata + image.rodata_size;
    // SAFETY: The caller validates image ranges and grants Builder exclusive mutation.
    unsafe {
        builder.map_range(base, range(image.physical_start, image.text_size)?, 0)?;
        builder.map_range(
            base + image.text_size,
            range(rodata, image.rodata_size)?,
            registers::PAGE_NO_EXECUTE,
        )?;
        builder.map_range(
            base + image.text_size + image.rodata_size,
            range(data, image.total_size - image.text_size - image.rodata_size)?,
            registers::PAGE_WRITE | registers::PAGE_NO_EXECUTE,
        )
    }
}

unsafe fn map_runtime_page(
    root: PhysicalAddress,
    virtual_address: u64,
    physical_address: u64,
    allocate: &mut dyn FnMut() -> Option<PhysicalAddress>,
) -> Result<(), Error> {
    let mut table = root;
    for level in 0..3 {
        let index = index(virtual_address, level);
        let entry = runtime_read(table, index)?;
        table = if entry == 0 {
            let child = allocate().ok_or(Error::RuntimeAllocation)?;
            let pointer = runtime_pointer(child)? as *mut u8;
            // SAFETY: The caller transfers a fresh writable page-table page.
            unsafe { write_bytes(pointer, 0, PAGE_SIZE as usize) };
            runtime_write(
                table,
                index,
                child.get() | registers::PAGE_PRESENT | registers::PAGE_WRITE,
            )?;
            child
        } else if entry & registers::PAGE_PRESENT != 0 && entry & registers::PAGE_LARGE == 0 {
            PhysicalAddress::new(pte_address(entry))
        } else {
            return Err(Error::Conflict);
        };
    }
    let index = index(virtual_address, 3);
    if runtime_read(table, index)? != 0 {
        return Err(Error::Conflict);
    }
    runtime_write(
        table,
        index,
        physical_address
            | registers::PAGE_PRESENT
            | registers::PAGE_WRITE
            | registers::PAGE_NO_EXECUTE,
    )
}

fn mapping(root: PhysicalAddress, address: u64) -> Result<Option<(u64, u64)>, Error> {
    if !canonical(address) {
        return Err(Error::InvalidAddress);
    }
    let mut table = root;
    for (level, size) in LEVEL_SIZES.iter().copied().enumerate() {
        let entry = runtime_read(table, index(address, level))?;
        if entry & registers::PAGE_PRESENT == 0 {
            return Ok(None);
        }
        if level == 3 || entry & registers::PAGE_LARGE != 0 {
            return Ok(Some((entry, size)));
        }
        table = PhysicalAddress::new(pte_address(entry));
    }
    Ok(None)
}

fn walk_leaf(root: PhysicalAddress, address: u64) -> Result<(PhysicalAddress, usize), Error> {
    let mut table = root;
    for level in 0..4 {
        let slot = index(address, level);
        let entry = runtime_read(table, slot)?;
        if entry & registers::PAGE_PRESENT == 0 {
            return Err(Error::Conflict);
        }
        if level == 3 {
            return Ok((table, slot));
        }
        if entry & registers::PAGE_LARGE != 0 {
            return Err(Error::Conflict);
        }
        table = PhysicalAddress::new(pte_address(entry));
    }
    Err(Error::Conflict)
}

#[cfg(feature = "crash-console")]
/// Inspects a mapping rooted at an externally supplied page table.
///
/// # Safety
///
/// `root` must identify a live, well-formed page-table hierarchy whose table
/// pages remain accessible through the kernel linear mapping.
pub unsafe fn inspect_mapping(root: u64, address: usize) -> Result<Option<Stage1Mapping>, Error> {
    let Some((entry, size)) = mapping(PhysicalAddress::new(root), address as u64)? else {
        return Ok(None);
    };
    Ok(Some(Stage1Mapping {
        virtual_start: address as u64 & !(size - 1),
        physical_start: pte_address(entry) & !(size - 1),
        size,
        readable: true,
        writable: entry & registers::PAGE_WRITE != 0,
        executable: entry & registers::PAGE_NO_EXECUTE == 0,
        memory_type: if entry & registers::PAGE_CACHE_DISABLE != 0 {
            Stage1MemoryType::Device
        } else {
            Stage1MemoryType::Normal
        },
    }))
}

fn stack_range(slot: usize, pages: usize) -> Result<(usize, usize, usize), Error> {
    if pages == 0 || pages + 1 > STACK_SLOT_PAGES {
        return Err(Error::InvalidRange);
    }
    let stride = STACK_SLOT_PAGES as u64 * PAGE_SIZE;
    let offset = (slot as u64)
        .checked_mul(stride)
        .ok_or(Error::AddressOverflow)?;
    let guard = STACK_ARENA_BASE
        .checked_add(offset)
        .ok_or(Error::AddressOverflow)?;
    guard
        .checked_add(stride - 1)
        .ok_or(Error::AddressOverflow)?;
    let bottom = guard.checked_add(PAGE_SIZE).ok_or(Error::AddressOverflow)?;
    let top = bottom
        .checked_add(pages as u64 * PAGE_SIZE)
        .ok_or(Error::AddressOverflow)?;
    Ok((guard as usize, bottom as usize, top as usize))
}

fn best_level(virtual_address: u64, physical_address: u64, remaining: u64) -> usize {
    LEVEL_SIZES
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, size)| {
            virtual_address.is_multiple_of(**size)
                && physical_address.is_multiple_of(**size)
                && remaining >= **size
        })
        .map(|(level, _)| level)
        .unwrap_or(3)
}

fn canonical(address: u64) -> bool {
    !((1 << 47)..0xffff_8000_0000_0000).contains(&address)
}

fn index(address: u64, level: usize) -> usize {
    ((address >> LEVEL_SHIFTS[level]) & (ENTRIES as u64 - 1)) as usize
}

fn pte_address(entry: u64) -> u64 {
    entry & 0x000f_ffff_ffff_f000
}

unsafe fn early_read(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = table.as_usize().ok_or(Error::InvalidAddress)? as *const u64;
    // SAFETY: The caller guarantees a directly accessible live table and slot.
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}

unsafe fn early_write(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = table.as_usize().ok_or(Error::InvalidAddress)? as *mut u64;
    // SAFETY: The caller guarantees exclusive access to a live table and slot.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}

fn runtime_pointer(table: PhysicalAddress) -> Result<usize, Error> {
    usize::try_from(registers::PML4_LINEAR_BASE + table.get()).map_err(|_| Error::InvalidAddress)
}

fn runtime_read(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    // SAFETY: Internal callers walk a live hierarchy and produce an in-range slot.
    Ok(unsafe { read_volatile((runtime_pointer(table)? as *const u64).add(slot)) })
}

fn runtime_write(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    // SAFETY: Internal callers serialize mutation of a live table and slot.
    unsafe { write_volatile((runtime_pointer(table)? as *mut u64).add(slot), value) };
    Ok(())
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
