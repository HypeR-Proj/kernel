// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guarded virtual kernel stacks and per-CPU exception-stack ownership.

use core::ptr::{read_volatile, write_bytes, write_volatile};

use crate::mm::allocator::heap::PageOwner;
use hyper::cpu::{CpuIndex, PerCpu};
use hyper::sync::InterruptSpinLock;
use {crate::mm::allocator::BuddyError, hyper::mm::PAGE_SIZE, hyper::mm::PhysicalAddress};

use super::page_block::PageBlock;

const WATERMARK_BYTE: u8 = 0xa5;
const WATERMARK_WORD: u64 = u64::from_ne_bytes([WATERMARK_BYTE; 8]);
const STACK_CANARY: u64 = 0x4859_5045_5253_544b;
const THREAD_STACK_BYTES: usize = hyper::config::KERNEL_STACK_SIZE_KB as usize * 1024;
const IRQ_STACK_BYTES: usize = hyper::config::IRQ_STACK_SIZE_KB as usize * 1024;
const EMERGENCY_STACK_BYTES: usize = hyper::config::EMERGENCY_STACK_SIZE_KB as usize * 1024;

const _: () = {
    assert!(THREAD_STACK_BYTES.is_power_of_two());
    assert!(IRQ_STACK_BYTES.is_power_of_two());
    assert!(EMERGENCY_STACK_BYTES.is_power_of_two());
    assert!(THREAD_STACK_BYTES >= 4 * PAGE_SIZE as usize);
    assert!(IRQ_STACK_BYTES >= 4 * PAGE_SIZE as usize);
    assert!(EMERGENCY_STACK_BYTES >= 4 * PAGE_SIZE as usize);
    assert!(THREAD_STACK_BYTES <= 64 * PAGE_SIZE as usize);
    assert!(IRQ_STACK_BYTES <= 64 * PAGE_SIZE as usize);
    assert!(EMERGENCY_STACK_BYTES <= 64 * PAGE_SIZE as usize);
};

type StackLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

pub(crate) const fn thread_stack_bytes() -> usize {
    THREAD_STACK_BYTES
}

// This is the sole runtime stage-1 mutation and virtual-slot allocator lock.
// On x86 its IRQ-mask policy services pending shootdowns while contended, so a
// CPU already executing with IF clear cannot block a synchronous invalidation.
static STACK_SLOTS: StackLock<StackSlots> = StackLock::new(StackSlots::new());
static CPU_STACKS: StackLock<PerCpu<CpuStacks>> = StackLock::new(PerCpu::new(
    [const { CpuStacks::new() }; hyper::cpu::MAX_CPUS],
));

pub(crate) fn serialize_stage1_mutation<R>(operation: impl FnOnce() -> R) -> R {
    STACK_SLOTS.with(|_| operation())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackKind {
    Thread,
    Irq,
    Emergency,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackStatistics {
    pub kind: StackKind,
    pub guard_page: usize,
    pub bottom: usize,
    pub top: usize,
    pub size: usize,
    pub used: usize,
    pub remaining: usize,
    pub canary_intact: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyPrepared,
    ArchitectureRejectedStack,
    InvalidConfiguration,
    InvalidCpuIndex,
    Mapping(super::memory::Error),
    OutOfStacks,
    Page(BuddyError),
}

impl From<BuddyError> for Error {
    fn from(error: BuddyError) -> Self {
        Self::Page(error)
    }
}

impl From<super::memory::Error> for Error {
    fn from(error: super::memory::Error) -> Self {
        Self::Mapping(error)
    }
}

struct StackSlots {
    occupied: alloc::vec::Vec<u64>,
}

impl StackSlots {
    const fn new() -> Self {
        Self {
            occupied: alloc::vec::Vec::new(),
        }
    }

    fn reserve(&mut self) -> Option<usize> {
        let (word, occupied) = self
            .occupied
            .iter_mut()
            .enumerate()
            .find(|(_, bits)| **bits != u64::MAX)?;
        let bit = (!*occupied).trailing_zeros() as usize;
        *occupied |= 1u64 << bit;
        Some(word * 64 + bit)
    }

    fn release(&mut self, slot: usize) {
        let word = self
            .occupied
            .get_mut(slot / 64)
            .unwrap_or_else(|| hyper::debug::invariant_failure("stack slot release out of range"));
        let mask = 1u64 << (slot % 64);
        if *word & mask == 0 {
            hyper::debug::invariant_failure("stack slot released twice");
        }
        *word &= !mask;
    }
}

/// Bitmap growth is prepared outside the IRQ-masked mapping lock. Racing
/// creators reuse the winner's capacity and free spare buffers after unlocking.
fn reserve_stack_slot() -> Result<usize, Error> {
    let mut prepared = alloc::vec::Vec::new();
    loop {
        let result = STACK_SLOTS.with(|slots| {
            if prepared.len() > slots.occupied.len() {
                prepared[..slots.occupied.len()].copy_from_slice(&slots.occupied);
                core::mem::swap(&mut prepared, &mut slots.occupied);
            }
            slots.reserve().ok_or(slots.occupied.len())
        });
        drop(core::mem::take(&mut prepared));
        match result {
            Ok(slot) => return Ok(slot),
            Err(words) => {
                let words = words.checked_mul(2).ok_or(Error::OutOfStacks)?.max(1);
                prepared
                    .try_reserve_exact(words)
                    .map_err(|_| Error::OutOfStacks)?;
                prepared.resize(words, 0);
            }
        }
    }
}

struct CpuStacks {
    preparing: bool,
    irq: Option<KernelStack>,
    emergency: Option<KernelStack>,
}

impl CpuStacks {
    const fn new() -> Self {
        Self {
            preparing: false,
            irq: None,
            emergency: None,
        }
    }
}

/// A physically backed stack exposed only through a guarded kernel VA slot.
pub(crate) struct KernelStack {
    block: PageBlock,
    slot: usize,
    pages: usize,
    mapping: crate::hal::memory::StackMapping,
    kind: StackKind,
}

impl KernelStack {
    pub fn allocate_thread() -> Result<Self, Error> {
        Self::allocate(StackKind::Thread, configured_pages(StackKind::Thread)?)
    }

    fn allocate(kind: StackKind, pages: usize) -> Result<Self, Error> {
        if !pages.is_power_of_two() {
            return Err(Error::InvalidConfiguration);
        }
        let slot = reserve_stack_slot()?;
        let block = match PageBlock::allocate(pages.trailing_zeros() as usize) {
            Ok(block) => block,
            Err(error) => {
                STACK_SLOTS.with(|slots| slots.release(slot));
                return Err(error.into());
            }
        };
        let mapping = match STACK_SLOTS.with(|_| map_stack(slot, block.physical(), pages)) {
            Ok(mapping) => mapping,
            Err(error) => {
                STACK_SLOTS.with(|slots| slots.release(slot));
                return Err(error);
            }
        };
        initialize_watermark(mapping.bottom, mapping.top);
        Ok(Self {
            block,
            slot,
            pages,
            mapping,
            kind,
        })
    }

    pub const fn top(&self) -> usize {
        self.mapping.top
    }

    pub const fn physical_top(&self) -> u64 {
        self.block.physical().get() + self.pages as u64 * PAGE_SIZE
    }

    pub const fn bounds(&self) -> (usize, usize) {
        (self.mapping.bottom, self.mapping.top)
    }

    pub fn statistics(&self) -> StackStatistics {
        stack_statistics(self.kind, self.mapping)
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let result = STACK_SLOTS.with(|slots| {
            let result = unmap_stack(self.slot, self.pages);
            if result.is_ok() {
                slots.release(self.slot);
            }
            result
        });
        if result.is_err() {
            // Destruction may occur under arbitrary allocator or scheduler
            // locks. Ordinary diagnostics could deadlock while a partially unmapped
            // stack owner is being released.
            hyper::debug::invariant_failure("mm::stack::drop invariant")
        }
    }
}

pub(crate) fn prepare_cpu(cpu: CpuIndex) -> Result<(), Error> {
    CPU_STACKS.with(|stacks| {
        let stacks = &mut stacks[cpu];
        if stacks.preparing || stacks.irq.is_some() || stacks.emergency.is_some() {
            return Err(Error::AlreadyPrepared);
        }
        stacks.preparing = true;
        Ok(())
    })?;

    let result = (|| {
        let irq = KernelStack::allocate(StackKind::Irq, configured_pages(StackKind::Irq)?)?;
        let emergency = KernelStack::allocate(
            StackKind::Emergency,
            configured_pages(StackKind::Emergency)?,
        )?;
        // SAFETY: The `preparing` claim excludes every concurrent setup for
        // this CPU. Both KernelStacks are moved into CPU_STACKS immediately
        // after publication and remain pinned for the CPU lifetime. SMP does
        // not start the target CPU until this function returns successfully.
        unsafe {
            crate::hal::exception::install_exception_stacks(cpu, irq.bounds(), emergency.bounds())
        }
        .map_err(|_| Error::ArchitectureRejectedStack)?;
        CPU_STACKS.with(|stacks| {
            stacks[cpu].irq = Some(irq);
            stacks[cpu].emergency = Some(emergency);
            stacks[cpu].preparing = false;
        });
        Ok(())
    })();

    if result.is_err() {
        CPU_STACKS.with(|stacks| stacks[cpu].preparing = false);
    }
    result
}

pub fn cpu_stack_statistics(cpu: usize) -> Option<(StackStatistics, StackStatistics)> {
    let cpu = CpuIndex::new(cpu)?;
    CPU_STACKS.with(|stacks| {
        // Check after LocalMask pins this execution to its CPU. Checking before
        // the lock would allow migration followed by a remote live-stack scan.
        if cpu != crate::cpu::current_index()? {
            return None;
        }
        let stacks = &stacks[cpu];
        Some((
            stacks.irq.as_ref()?.statistics(),
            stacks.emergency.as_ref()?.statistics(),
        ))
    })
}

pub fn guard_page_is_unmapped(statistics: StackStatistics) -> Result<bool, Error> {
    STACK_SLOTS
        .with(|_| {
            crate::boot::with_boot_state(|state| {
                // SAFETY: STACK_SLOTS serializes every runtime stage-1 stack map
                // mutation, and boot state pins the hierarchy.
                unsafe { state.memory.address_is_mapped(statistics.guard_page) }
            })
        })
        .map(|mapped| !mapped)
        .map_err(Error::from)
}

pub(crate) fn exception_stack_bounds(cpu: usize, pointer: usize) -> Option<(usize, usize)> {
    let cpu = CpuIndex::new(cpu)?;
    // Crash and PMU unwinding may interrupt a stack-registry update. They must
    // neither wait for that lock nor poll cross-CPU work while sampling.
    CPU_STACKS
        .try_with(|stacks| {
            let stacks = &stacks[cpu];
            [&stacks.irq, &stacks.emergency]
                .into_iter()
                .flatten()
                .map(KernelStack::bounds)
                .find(|(bottom, top)| *bottom <= pointer && pointer <= *top)
        })
        .flatten()
}

/// Discards the live call chain and restarts execution at a clean stack top.
///
/// # Safety
///
/// `bounds` must identify an exclusively owned stack that contains no live
/// values needed after the transfer. Local interrupts must be masked, and
/// `callback` must never return.
pub(crate) unsafe fn reset_and_enter(
    bounds: (usize, usize),
    callback: extern "C" fn(usize) -> !,
    argument: usize,
) -> ! {
    // SAFETY: The caller supplies an exclusive destination stack and
    // permanently abandons the current call chain before it is refilled.
    unsafe {
        crate::hal::context::reset_stack_and_enter(
            bounds.0,
            bounds.1,
            WATERMARK_WORD,
            STACK_CANARY,
            callback,
            argument,
        )
    }
}

fn configured_pages(kind: StackKind) -> Result<usize, Error> {
    let bytes = match kind {
        StackKind::Thread => THREAD_STACK_BYTES,
        StackKind::Irq => IRQ_STACK_BYTES,
        StackKind::Emergency => EMERGENCY_STACK_BYTES,
    };
    if !bytes.is_multiple_of(PAGE_SIZE as usize) {
        return Err(Error::InvalidConfiguration);
    }
    let pages = bytes / PAGE_SIZE as usize;
    if pages == 0 || pages > 64 || !pages.is_power_of_two() {
        return Err(Error::InvalidConfiguration);
    }
    Ok(pages)
}

fn map_stack(
    slot: usize,
    physical: PhysicalAddress,
    pages: usize,
) -> Result<crate::hal::memory::StackMapping, Error> {
    crate::boot::with_boot_state(|state| {
        // SAFETY: allocate_page_table returns exclusive, zeroed, aligned
        // PageTable-owned pages from the permanent linear map. They are never
        // reclaimed, and STACK_SLOTS serializes all runtime stack mappings.
        unsafe {
            state
                .memory
                .map_kernel_stack(slot, physical, pages, &mut allocate_page_table)
        }
    })
    .map_err(Error::from)
}

fn unmap_stack(slot: usize, pages: usize) -> Result<(), Error> {
    crate::boot::with_boot_state(|state| {
        // SAFETY: Caller holds STACK_SLOTS, KernelStack::drop runs only after
        // abandoning the stack, and the slot cannot be reused until success.
        unsafe { state.memory.unmap_kernel_stack(slot, pages) }
    })
    .map_err(Error::from)
}

fn allocate_page_table() -> Option<PhysicalAddress> {
    let physical = crate::mm::allocator::policy::GLOBAL_ALLOCATOR
        .allocate_pages_for(0, PageOwner::PageTable)
        .ok()?;
    let Some(virtual_address) = super::memory::linear_address(physical.get()) else {
        // SAFETY: `physical` is the fresh order-zero allocation obtained just
        // above and has not been exposed or mapped anywhere.
        let _ = unsafe {
            crate::mm::allocator::policy::GLOBAL_ALLOCATOR.deallocate_pages_for(
                physical,
                0,
                PageOwner::PageTable,
            )
        };
        return None;
    };
    // SAFETY: The new page is exclusively PageTable-owned and fully covered
    // by the permanent writable linear map. It remains allocated permanently.
    unsafe { write_bytes(virtual_address as *mut u8, 0, PAGE_SIZE as usize) };
    Some(physical)
}

fn initialize_watermark(bottom: usize, top: usize) {
    let size = top - bottom;
    // SAFETY: The mapping is a new, exclusive writable stack allocation.
    unsafe {
        write_bytes(bottom as *mut u8, WATERMARK_BYTE, size);
        write_volatile(bottom as *mut u64, STACK_CANARY);
    }
}

fn stack_statistics(kind: StackKind, mapping: crate::hal::memory::StackMapping) -> StackStatistics {
    // SAFETY: Stack mappings remain live while their owning KernelStack exists.
    let canary = unsafe { read_volatile(mapping.bottom as *const u64) };
    let watermark = mapping.bottom + core::mem::size_of::<u64>();
    let mut cursor = watermark;
    while cursor < mapping.top {
        // SAFETY: cursor stays inside the live stack mapping.
        if unsafe { read_volatile(cursor as *const u8) } != WATERMARK_BYTE {
            break;
        }
        cursor += 1;
    }
    let size = mapping.top - mapping.bottom;
    let unused = cursor - watermark;
    let used = size
        .saturating_sub(core::mem::size_of::<u64>())
        .saturating_sub(unused);
    StackStatistics {
        kind,
        guard_page: mapping.guard_page,
        bottom: mapping.bottom,
        top: mapping.top,
        size,
        used,
        remaining: unused,
        canary_intact: canary == STACK_CANARY,
    }
}
