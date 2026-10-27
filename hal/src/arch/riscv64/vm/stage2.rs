// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Sv39x4 guest-stage translation for the RISC-V hypervisor extension.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use hyper::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use {
    hyper_vm_models::translation::ActiveMappingError,
    hyper_vm_models::translation::Stage2PagePermissions,
    hyper_vm_models::translation::publish_active_mapping,
};

use crate::arch::riscv64::memory::Riscv64AddressTranslation;
use crate::arch::riscv64::registers;
use hyper::hal::memory::AddressTranslation;

const LEVEL_SHIFTS: [u64; 3] = [30, 21, 12];
const LEVEL_SIZES: [u64; 3] = [1 << 30, 1 << 21, PAGE_SIZE];
const ROOT_ENTRIES: u64 = 2048;
static VMID_BITS: AtomicU8 = AtomicU8::new(14);
static DISCOVERED: AtomicBool = AtomicBool::new(false);
static PROBE_ROOT_PHYSICAL: AtomicU64 = AtomicU64::new(0);
#[repr(C, align(16384))]
struct ProbeRoot([u64; 2048]);
// Permanently retained, immutable invalid entries. This root makes even a
// speculative walk during VMID probing refer only to owned storage.
static PROBE_ROOT: ProbeRoot = ProbeRoot([0; 2048]);

/// Captures the linked table's physical address before HS translation starts.
///
/// # Safety
/// Must run in the identity-addressed primary boot phase, before CPU admission.
pub(crate) unsafe fn prepare_discovery() {
    PROBE_ROOT_PHYSICAL.store(core::ptr::addr_of!(PROBE_ROOT) as u64, Ordering::Release);
}

/// CPU admission precedes publication of every VM. All admitted harts contribute
/// to this minimum; no hotplug may lower it after guest roots have been created.
pub(crate) fn discover_local() -> bool {
    let root = PROBE_ROOT_PHYSICAL.load(Ordering::Acquire);
    if root == 0 || !root.is_multiple_of(4 * PAGE_SIZE) {
        return false;
    }
    let probe =
        registers::HGATP_MODE_SV39X4 | registers::HGATP_VMID_MASK | (root >> registers::PAGE_SHIFT);
    let previous: u64;
    let probed: u64;
    // SAFETY: Admission runs in HS mode with interrupts masked and no guest
    // selected on this hart. The Sv39x4 probe uses an owned empty root (Bare
    // with a nonzero VMID is unspecified). Both transitions are fenced, and
    // the previous register is restored exactly.
    unsafe {
        asm!(
            ".option push", ".option arch, +h",
            "csrr {previous}, hgatp",
            "csrw hgatp, {mask}",
            "hfence.gvma zero, zero",
            "csrr {probed}, hgatp",
            "csrw hgatp, {previous}",
            "hfence.gvma zero, zero",
            ".option pop",
            previous = out(reg) previous,
            probed = out(reg) probed,
            mask = in(reg) probe,
            options(nostack),
        );
    }
    // Admission must not be invoked while a guest translation is selected.
    if previous != 0
        || probed >> 60 != 8
        || probed & ((1 << 44) - 1) != root >> registers::PAGE_SHIFT
    {
        return false;
    }
    let bits =
        ((probed & registers::HGATP_VMID_MASK) >> registers::HGATP_VMID_SHIFT).count_ones() as u8;
    VMID_BITS.fetch_min(bits, Ordering::AcqRel);
    DISCOVERED.store(true, Ordering::Release);
    true
}

pub(crate) fn identifier_bits() -> Result<u8, Error> {
    if !DISCOVERED.load(Ordering::Acquire) {
        return Err(Error::NotInitialized);
    }
    let bits = VMID_BITS.load(Ordering::Acquire);
    Ok(crate::arch::riscv64::memory::translation_id_policy::guest_software_bits(bits))
}

fn hardware_identifier(identifier: u16) -> u16 {
    crate::arch::riscv64::memory::translation_id_policy::guest_hardware_identifier(
        VMID_BITS.load(Ordering::Acquire),
        identifier,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestStage2RetirementRequest {
    hgatp: u64,
}

const _: () = {
    assert!(
        normal_leaf_permissions(Stage2PagePermissions::ReadWrite) & registers::PTE_EXECUTE == 0
    );
    assert!(
        normal_leaf_permissions(Stage2PagePermissions::ReadWriteExecute) & registers::PTE_EXECUTE
            != 0
    );
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    Allocation,
    Conflict,
    InvalidAddress,
    InvalidRange,
    InvalidVmid,
    NotInitialized,
    ActiveOwner,
}

pub struct Stage2AddressSpace {
    root: PhysicalAddress,
    vmid: u16,
}

impl Stage2AddressSpace {
    /// Selects a leased hardware tag independently of the hierarchy's lifetime.
    ///
    /// # Safety
    /// The caller pins this identifier through every hardware use and serializes
    /// selection with hierarchy mutation. Existing pins must identify the same tag.
    pub unsafe fn bind_identifier(&mut self, vmid: u16) -> Result<(), Error> {
        if vmid == 0 || u64::from(vmid) >= 1 << identifier_bits()? {
            return Err(Error::InvalidVmid);
        }
        self.vmid = vmid;
        Ok(())
    }

    /// Opportunistic RAM blocks are not implemented by this backend.
    pub const fn normal_block_size() -> Option<u64> {
        None
    }

    /// Declines block installation; callers retain the 4 KiB mapping path.
    ///
    /// # Safety
    /// The caller must follow the active page-mapping ownership contract.
    pub unsafe fn try_map_normal_block_active(
        &mut self,
        _ipa: u64,
        _physical: u64,
        _permissions: Stage2PagePermissions,
        _allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<bool, ActiveMappingError<Error>> {
        Ok(false)
    }

    /// Validates a page-granular revocation range; no splitting is required.
    ///
    /// # Safety
    /// Caller serializes hierarchy mutation through the subsequent clear.
    pub unsafe fn prepare_clear_normal_range(
        &mut self,
        ipa: u64,
        size: u64,
        _allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_range(ipa, ipa, size)?;
        for address in (ipa..ipa + size).step_by(PAGE_SIZE as usize) {
            // SAFETY: Exclusive inspection; no descriptor stores in this pass.
            unsafe {
                self.visit_clear_page(address, false)?;
            }
        }
        Ok(())
    }

    /// Clears normal pages, retaining backing for subsequent invalidation.
    ///
    /// # Safety
    /// The caller excludes mutation and retains all removed backing until the
    /// existing architecture-specific synchronization completes.
    pub unsafe fn clear_normal_range(&mut self, ipa: u64, size: u64) -> Result<(), Error> {
        validate_range(ipa, ipa, size)?;
        for clear in [false, true] {
            for address in (ipa..ipa + size).step_by(PAGE_SIZE as usize) {
                // SAFETY: Validate the whole range before revoking any leaf;
                // mutation and backing retention follow the outer contract.
                unsafe {
                    self.visit_clear_page(address, clear)?;
                }
            }
        }
        Ok(())
    }

    /// Detaches empty child tables without freeing their physical storage.
    ///
    /// # Safety
    /// The caller serializes all hierarchy writers and retains every detached
    /// page until a subsequent all-consumer translation invalidation completes.
    /// Detached parent slots must not be rebuilt before that invalidation.
    /// `retire` must not free pages or re-enter hierarchy mutation.
    pub unsafe fn detach_empty_tables(
        &mut self,
        retire: &mut impl FnMut(PhysicalAddress),
    ) -> Result<(), Error> {
        // SAFETY: The outer contract retains the complete tree and detached children.
        unsafe {
            detach_empty(self.root, 0, retire)?;
        }
        Ok(())
    }

    pub fn required_table_pages(ipa: u64, size: u64) -> Result<usize, Error> {
        validate_range(ipa, ipa, size)?;
        let end = ipa.checked_add(size).ok_or(Error::AddressOverflow)?;
        let level1 = covering_regions(ipa, end, LEVEL_SIZES[0])?;
        let level2 = covering_regions(ipa, end, LEVEL_SIZES[1])?;
        4usize
            .checked_add(level1)
            .and_then(|pages| pages.checked_add(level2))
            .ok_or(Error::AddressOverflow)
    }

    /// Creates a guest address space backed by pages returned by `allocator`.
    ///
    /// # Safety
    ///
    /// Every successful allocation must return a uniquely owned, zeroed,
    /// suitably aligned group of RAM pages. Those pages must remain live and
    /// accessible through the kernel linear mapping for the lifetime of this
    /// address space.
    pub unsafe fn new(
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<Self, Error> {
        let root = allocator(4, 4).ok_or(Error::Allocation)?;
        if !root.get().is_multiple_of(4 * PAGE_SIZE) {
            return Err(Error::InvalidAddress);
        }
        Ok(Self { root, vmid: 0 })
    }

    pub const fn root_address(&self) -> u64 {
        self.root.get()
    }

    #[allow(dead_code)]
    /// Maps normal guest memory.
    ///
    /// # Safety
    ///
    /// The address-space pages supplied to `new` and any pages returned by
    /// `allocator` must satisfy `new`'s ownership and accessibility contract.
    /// The caller must also serialize page-table mutation.
    pub unsafe fn map_normal(
        &mut self,
        ipa: u64,
        physical: u64,
        size: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_range(ipa, physical, size)?;
        let mut offset = 0;
        while offset < size {
            let level = best_level(ipa + offset, physical + offset, size - offset);
            // SAFETY: The method contract covers all allocator pages and serialized mutation.
            unsafe {
                self.map_leaf(
                    ipa + offset,
                    physical + offset,
                    level,
                    Stage2PagePermissions::ReadWriteExecute,
                    allocator,
                )?
            };
            offset += LEVEL_SIZES[level];
        }
        Ok(())
    }

    /// Maps one normal guest page.
    ///
    /// # Safety
    ///
    /// The page-table allocation and serialization requirements from
    /// `map_normal` apply.
    pub unsafe fn map_normal_page(
        &mut self,
        ipa: u64,
        physical: u64,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_range(ipa, physical, PAGE_SIZE)?;
        // SAFETY: The method contract covers allocator pages and serialized mutation.
        unsafe { self.map_leaf(ipa, physical, 2, permissions, allocator) }
    }

    /// Maps one page and publishes the update for the active guest hierarchy.
    ///
    /// # Safety
    ///
    /// The allocation, retention, and serialization requirements of
    /// `map_normal_page` apply. This hierarchy must be active on this hart,
    /// with an exclusive execution lease excluding concurrent guest access.
    pub unsafe fn map_normal_page_active(
        &mut self,
        ipa: u64,
        physical: u64,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), ActiveMappingError<Error>> {
        publish_active_mapping(
            self,
            |stage2| {
                // SAFETY: This function has the same allocator and
                // serialization contract.
                unsafe { stage2.map_normal_page(ipa, physical, permissions, allocator) }
            },
            |stage2| {
                // SAFETY: The caller guarantees this VMID is active.
                unsafe { invalidate(ipa, stage2.vmid) }
            },
        )
    }

    /// Removes one normal 4 KiB leaf, retaining all intermediate tables.
    ///
    /// # Safety
    /// The caller serializes updates and retains the old backing until every
    /// possible hart has acknowledged the subsequent live invalidation.
    unsafe fn visit_clear_page(&mut self, ipa: u64, clear: bool) -> Result<bool, Error> {
        if !ipa.is_multiple_of(PAGE_SIZE) || ipa >= registers::STAGE2_IPA_LIMIT {
            return Err(Error::InvalidAddress);
        }
        let mut table = self.root;
        for level in 0..2 {
            // SAFETY: This hierarchy retains its root and every parent table.
            let entry = unsafe { read_entry(table, index(ipa, level))? };
            if entry == 0 {
                return Ok(false);
            }
            if entry & registers::PTE_VALID == 0
                || entry & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE)
                    != 0
            {
                return Err(Error::Conflict);
            }
            table = PhysicalAddress::new(pte_address(entry));
        }
        // SAFETY: The preceding walk validated a retained final-level table.
        let entry = unsafe { read_entry(table, index(ipa, 2))? };
        if entry == 0 {
            return Ok(false);
        }
        let (pointer, _) = self.normal_page_leaf(ipa)?;
        if clear {
            // SAFETY: The caller excludes mutation and retains backing through TLBI.
            unsafe { write_volatile(pointer, 0) };
        }
        Ok(true)
    }

    /// Grants execute permission to an inactive normal-memory leaf.
    ///
    /// # Safety
    /// The caller excludes guest execution and serializes hierarchy mutation.
    pub unsafe fn make_normal_page_executable(
        &mut self,
        ipa: u64,
        _allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        self.install_execute_permission(ipa)
    }

    /// Grants execute permission to an active normal-memory leaf and flushes
    /// the affected guest translation on the current hart.
    ///
    /// # Safety
    ///
    /// The caller must own the only active vCPU for this address space.
    pub unsafe fn make_normal_page_executable_active(
        &mut self,
        ipa: u64,
        _allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), ActiveMappingError<Error>> {
        publish_active_mapping(
            self,
            |stage2| stage2.install_execute_permission(ipa),
            |stage2| {
                // SAFETY: The caller guarantees this VMID is active and the
                // exclusive execution lease excludes another consumer.
                unsafe { invalidate(ipa, stage2.vmid) }
            },
        )
    }

    /// Invalidates one page in the currently active guest hierarchy.
    ///
    /// # Safety
    ///
    /// This hierarchy and its VMID must remain retained and active on the
    /// current hart. The caller must serialize invalidation with activation
    /// and retain affected backing until all required harts acknowledge it.
    pub unsafe fn invalidate_page_active(&self, ipa: u64) -> Result<(), Error> {
        if !ipa.is_multiple_of(PAGE_SIZE) || ipa >= registers::STAGE2_IPA_LIMIT {
            return Err(Error::InvalidAddress);
        }
        // SAFETY: The caller guarantees this VMID is the active address space.
        unsafe { invalidate(ipa, self.vmid) }
    }

    #[allow(dead_code)]
    /// Maps a guest device range.
    ///
    /// # Safety
    ///
    /// The page-table allocation and serialization requirements from
    /// `map_normal` apply.
    pub unsafe fn map_device(
        &mut self,
        ipa: u64,
        physical: u64,
        size: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        // SAFETY: This function has the same allocator and serialization contract.
        unsafe { self.map_normal(ipa, physical, size, allocator) }
    }

    pub(crate) fn retirement_request(&self) -> GuestStage2RetirementRequest {
        GuestStage2RetirementRequest {
            hgatp: self.hgatp(),
        }
    }

    fn hgatp(&self) -> u64 {
        registers::HGATP_MODE_SV39X4
            | (u64::from(hardware_identifier(self.vmid)) << registers::HGATP_VMID_SHIFT)
            | (self.root.get() >> registers::PAGE_SHIFT)
    }

    /// Tests the current hart's actual selection, independently of a retained
    /// translation-cache epoch. Detachment may clear HGATP without changing
    /// that epoch, so residency alone cannot authorize skipping activation.
    pub(crate) fn is_active_local(&self) -> bool {
        // Untagged execution must pass through the assembly switch fences on
        // every admission, even when the retained root selection still matches.
        if !crate::arch::riscv64::memory::translation_id_policy::guest_selection_cacheable(
            VMID_BITS.load(Ordering::Acquire),
        ) {
            return false;
        }
        let current: u64;
        // SAFETY: The selected RISC-V backend runs in HS mode with H admitted.
        // This read does not grant ownership or alter the current selection.
        unsafe {
            asm!("csrr {current}, hgatp", current = out(reg) current, options(nostack));
        }
        current == self.hgatp()
    }

    /// Selects this complete guest translation hierarchy on the current hart.
    ///
    /// # Safety
    ///
    /// The hierarchy, mapped backing, and VMID must remain retained until
    /// acknowledged retirement. Activation must be serialized with guest
    /// execution and local translation updates on the current hart.
    pub unsafe fn activate(&self) {
        let value = self.hgatp();
        // SAFETY: The caller guarantees the hierarchy is complete and activation is serialized.
        unsafe { riscv64_activate_stage2(value) };
    }

    unsafe fn map_leaf(
        &mut self,
        ipa: u64,
        physical: u64,
        leaf_level: usize,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        let mut table = self.root;
        for level in 0..leaf_level {
            let slot = index(ipa, level);
            // SAFETY: The method contract guarantees every walked table is live and mapped.
            let entry = unsafe { read_entry(table, slot)? };
            table = if entry == 0 {
                let child = allocator(1, 1).ok_or(Error::Allocation)?;
                // SAFETY: The allocator contract provides a fresh live child table.
                unsafe { write_entry(table, slot, table_pte(child.get()))? };
                child
            } else if entry & registers::PTE_VALID != 0
                && entry & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE)
                    == 0
            {
                PhysicalAddress::new(pte_address(entry))
            } else {
                return Err(Error::Conflict);
            };
        }
        let slot = index(ipa, leaf_level);
        let value = (physical >> 2)
            | registers::PTE_VALID
            | registers::PTE_READ
            | registers::PTE_WRITE
            | registers::PTE_USER
            | registers::PTE_ACCESSED
            | registers::PTE_DIRTY
            | normal_leaf_permissions(permissions);
        // SAFETY: The walk established a live mapped leaf table.
        let existing = unsafe { read_entry(table, slot)? };
        if existing != 0 && existing != value {
            return Err(Error::Conflict);
        }
        // SAFETY: Mutation is serialized and the leaf table is live and mapped.
        unsafe { write_entry(table, slot, value) }
    }

    fn install_execute_permission(&mut self, ipa: u64) -> Result<(), Error> {
        let (pointer, entry) = self.normal_page_leaf(ipa)?;
        if entry & registers::PTE_EXECUTE != 0 {
            return Ok(());
        }
        // SAFETY: Serialized mutation owns this validated final-level slot.
        unsafe { write_volatile(pointer, entry | registers::PTE_EXECUTE) };
        Ok(())
    }

    fn normal_page_leaf(&self, ipa: u64) -> Result<(*mut u64, u64), Error> {
        if !ipa.is_multiple_of(PAGE_SIZE) || ipa >= registers::STAGE2_IPA_LIMIT {
            return Err(Error::InvalidAddress);
        }
        let mut table = self.root;
        for level in 0..2 {
            // SAFETY: The root and every valid non-leaf child are retained by
            // this address space.
            let entry = unsafe { read_entry(table, index(ipa, level))? };
            if entry & registers::PTE_VALID == 0
                || entry & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE)
                    != 0
            {
                return Err(Error::Conflict);
            }
            table = PhysicalAddress::new(pte_address(entry));
        }
        let pointer = table_pointer(table)? as *mut u64;
        // SAFETY: The walk validated the live final-level table.
        let pointer = unsafe { pointer.add(index(ipa, 2)) };
        // SAFETY: Address-space mutation is serialized by the caller.
        let entry = unsafe { read_volatile(pointer) };
        let required =
            registers::PTE_VALID | registers::PTE_READ | registers::PTE_WRITE | registers::PTE_USER;
        if entry & required != required {
            return Err(Error::Conflict);
        }
        Ok((pointer, entry))
    }
}

const fn normal_leaf_permissions(permissions: Stage2PagePermissions) -> u64 {
    if permissions.is_executable() {
        registers::PTE_EXECUTE
    } else {
        0
    }
}

fn index(ipa: u64, level: usize) -> usize {
    let mask = if level == 0 { ROOT_ENTRIES - 1 } else { 511 };
    ((ipa >> LEVEL_SHIFTS[level]) & mask) as usize
}

#[allow(dead_code)]
fn best_level(ipa: u64, physical: u64, remaining: u64) -> usize {
    LEVEL_SIZES
        .iter()
        .position(|size| {
            ipa.is_multiple_of(*size) && physical.is_multiple_of(*size) && remaining >= *size
        })
        .unwrap_or(2)
}

fn table_pte(address: u64) -> u64 {
    (address >> 2) | registers::PTE_VALID
}
fn pte_address(entry: u64) -> u64 {
    (entry >> 10) << 12
}

unsafe fn read_entry(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = table_pointer(table)? as *const u64;
    // SAFETY: The caller guarantees a live table and `slot` is produced by index().
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}

unsafe fn write_entry(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = table_pointer(table)? as *mut u64;
    // SAFETY: The caller guarantees exclusive table mutation and an in-range slot.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}

fn table_pointer(table: PhysicalAddress) -> Result<usize, Error> {
    Riscv64AddressTranslation::linear_address(table)
        .and_then(|address| usize::try_from(address.get()).ok())
        .ok_or(Error::InvalidAddress)
}

fn validate_range(ipa: u64, physical: u64, size: u64) -> Result<(), Error> {
    if size == 0
        || !ipa.is_multiple_of(PAGE_SIZE)
        || !physical.is_multiple_of(PAGE_SIZE)
        || !size.is_multiple_of(PAGE_SIZE)
    {
        return Err(Error::InvalidRange);
    }
    let end = ipa.checked_add(size).ok_or(Error::AddressOverflow)?;
    let physical_end = physical.checked_add(size).ok_or(Error::AddressOverflow)?;
    if end > registers::STAGE2_IPA_LIMIT || physical_end > registers::PHYSICAL_ADDRESS_LIMIT {
        return Err(Error::InvalidAddress);
    }
    Ok(())
}

fn covering_regions(start: u64, end: u64, span: u64) -> Result<usize, Error> {
    let first = start / span;
    let last = end.checked_sub(1).ok_or(Error::InvalidRange)? / span;
    usize::try_from(last - first + 1).map_err(|_| Error::AddressOverflow)
}

unsafe fn invalidate(ipa: u64, vmid: u16) -> Result<(), Error> {
    let start = usize::try_from(ipa).map_err(|_| Error::InvalidAddress)?;
    // The exclusive execution claim proves that only this hart can consume the
    // active VMID. A future migration observes the incremented translation
    // epoch and performs a full local activation fence on its destination, so
    // broadcasting an SBI remote fence on every demand fault is unnecessary.
    // SAFETY: The caller guarantees this VMID is active and invalidation is
    // serialized.
    let vmid = hardware_identifier(vmid);
    if vmid == 0 {
        // All logical owners share tag zero in the narrow-hardware fallback.
        invalidate_namespace_local();
    } else {
        // SAFETY: The validated hardware identifier belongs to this active root.
        unsafe { riscv64_invalidate_stage2_page(start >> 2, usize::from(vmid)) };
    }
    Ok(())
}

/// Parks a retired root without selecting its potentially reassigned VMID.
pub(crate) fn retire_root_local(root: u64) {
    if crate::arch::riscv64::vm::vcpu::stage2_root_is_active(root) {
        hyper::debug::invariant_failure("retiring guest root still has execution owner");
    }
    let current: u64;
    // SAFETY: HS reads its selected guest root with execution serialized.
    unsafe { asm!("csrr {current}, hgatp", current = out(reg) current, options(nostack)) };
    if (current & ((1 << 44) - 1)) << registers::PAGE_SHIFT == root {
        // SAFETY: Global retirement excludes consumers of this matching root.
        unsafe {
            asm!(
                ".option push",
                ".option arch, +h",
                "csrw vsatp, zero",
                "csrw hgatp, zero",
                ".option pop",
                options(nostack)
            );
        }
    }
    invalidate_namespace_local();
}

pub(crate) fn publish_changes() {
    // SAFETY: Order page-table stores before publishing cross-hart fence work.
    unsafe { asm!("fence rw, rw", options(nostack)) };
}

pub(crate) fn synchronize_local(request: GuestStage2RetirementRequest) {
    let vmid = (request.hgatp & registers::HGATP_VMID_MASK) >> registers::HGATP_VMID_SHIFT;
    if vmid == 0 {
        invalidate_namespace_local();
        return;
    }
    // SAFETY: The request retains its translation identity. HFENCE.GVMA targets
    // the supplied VMID without changing the current HGATP or VSATP selection.
    unsafe {
        asm!(
            ".option push",
            ".option arch, +h",
            "hfence.gvma zero, {vmid}",
            ".option pop",
            vmid = in(reg) vmid,
            options(nostack)
        );
    }
}

unsafe extern "C" {
    fn riscv64_activate_stage2(value: u64);
    fn riscv64_invalidate_stage2_page(guest_physical_operand: usize, vmid: usize);
}

/// Flushes every guest VMID locally before admitting a new allocator epoch.
pub fn invalidate_namespace_local() {
    // SAFETY: The caller runs in HS with guest execution stopped and IRQs masked.
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +h",
            "fence rw, rw",
            "hfence.gvma zero, zero",
            "hfence.vvma zero, zero",
            ".option pop",
            options(nostack)
        );
    }
}

unsafe fn detach_empty(
    table: PhysicalAddress,
    level: usize,
    retire: &mut impl FnMut(PhysicalAddress),
) -> Result<bool, Error> {
    let pointer = table_pointer(table)? as *mut u64;
    let count = if level == 0 {
        ROOT_ENTRIES as usize
    } else {
        512
    };
    let mut empty = true;
    for slot in 0..count {
        // SAFETY: The root owns 2048 entries; subordinate tables each own 512.
        let entry = unsafe { pointer.add(slot) };
        // SAFETY: Serialized software mutation, with all table owners retained.
        let descriptor = unsafe { read_volatile(entry) };
        if descriptor == 0 {
            continue;
        }
        if level < 2
            && descriptor & registers::PTE_VALID != 0
            && descriptor & (registers::PTE_READ | registers::PTE_WRITE | registers::PTE_EXECUTE)
                == 0
        {
            let child = PhysicalAddress::new(pte_address(descriptor));
            // SAFETY: The valid non-leaf descriptor names a retained child table.
            if unsafe { detach_empty(child, level + 1, retire)? } {
                // SAFETY: Retire only detaches; no backing is freed before HFENCE completion.
                unsafe { write_volatile(entry, 0) };
                retire(child);
                continue;
            }
        }
        empty = false;
    }
    Ok(empty)
}
