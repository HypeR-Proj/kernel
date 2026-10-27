// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` stage-2 translation tables and activation.

pub(crate) mod retirement;

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use {
    hyper_vm_models::translation::ActiveMappingError,
    hyper_vm_models::translation::Stage2PagePermissions,
    hyper_vm_models::translation::publish_active_mapping,
};

use {
    crate::arch::aarch64::memory, crate::arch::aarch64::memory::address,
    crate::arch::aarch64::registers,
};

// An up-to-39-bit IPA started at level 1 fits in one 4 KiB root table. A
// 40-bit IPA would require two concatenated, 8 KiB-aligned root tables.
const _: () = {
    assert!(
        registers::TRANSLATION_TABLE_ENTRY_COUNT_4K as u64 * registers::STAGE2_LEVEL_SIZES_4K[0]
            >= address::STAGE2_IPA_LIMIT
    );
    assert!(registers::VTCR_EL2_GUEST_BASE & registers::VTCR_EL2_T0SZ_MASK == 0);
    assert!(
        normal_memory_attributes(Stage2PagePermissions::ReadWrite) & registers::STAGE2_DESC_XN != 0
    );
    assert!(
        normal_memory_attributes(Stage2PagePermissions::ReadWriteExecute)
            & registers::STAGE2_DESC_XN
            == 0
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
}

#[derive(Clone, Copy)]
enum MemoryType {
    Normal,
    Device,
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
        if vmid == 0 || u32::from(vmid) >= (1_u32 << address::capabilities().vmid_bits) {
            return Err(Error::InvalidVmid);
        }
        self.vmid = vmid;
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
        validate_ipa_range(ipa, size)?;
        let end = ipa.checked_add(size).ok_or(Error::AddressOverflow)?;
        let level1 = covering_regions(ipa, end, registers::STAGE2_LEVEL_SIZES_4K[0])?;
        let level2 = covering_regions(ipa, end, registers::STAGE2_LEVEL_SIZES_4K[1])?;
        let device_tables = if crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some() {
            2
        } else {
            0
        };
        1usize
            .checked_add(device_tables)
            .and_then(|pages| pages.checked_add(level1))
            .and_then(|pages| pages.checked_add(level2))
            .ok_or(Error::AddressOverflow)
    }

    /// Creates an empty stage-2 hierarchy.
    ///
    /// # Safety
    ///
    /// Every page returned by `allocator` must be uniquely owned by this
    /// hierarchy, zero-initialized, aligned as requested, accessible through
    /// the permanent host linear map, and kept alive until this address space
    /// can no longer be active or accessed.
    pub unsafe fn new(
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<Self, Error> {
        let root = allocator(1, 1).ok_or(Error::Allocation)?;
        validate_table(root)?;
        let mut result = Self { root, vmid: 0 };
        if let Some(physical) = crate::arch::aarch64::vm::vgic::v2::guest_physical() {
            // SAFETY: Only the guest virtual CPU interface is exposed, never
            // GICC/GICH. It is banked by CPU and switched with the vCPU context.
            // Only the first page is exposed: the second page contains DIR,
            // which must trap so software-resident active interrupts can retire.
            // The root is unpublished and allocator ownership is unchanged.
            unsafe {
                result.map_device(hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_GICV2_CPU_BASE, physical, 0x1000, allocator)?;
            }
        }
        Ok(result)
    }

    pub const fn root_address(&self) -> u64 {
        self.root.get()
    }

    pub(crate) fn retirement_request(&self) -> crate::arch::aarch64::GuestStage2RetirementRequest {
        let vttbr = (u64::from(self.vmid) << registers::VTTBR_EL2_VMID_SHIFT) | self.root.get();
        crate::arch::aarch64::GuestStage2RetirementRequest::new(
            vttbr,
            address::capabilities().stage2_vtcr_el2(),
        )
    }

    /// Tests the local root and translation controls without changing the VHE
    /// host regime. Retained residency does not imply a still-selected root.
    pub(crate) fn is_active_local(&self) -> bool {
        let vttbr: u64;
        let vtcr: u64;
        let hcr: u64;
        // SAFETY: This backend executes at EL2; these register reads have no
        // side effects and do not enter the lower-EL translation regime.
        unsafe {
            asm!(
                "mrs {vttbr}, VTTBR_EL2",
                "mrs {vtcr}, VTCR_EL2",
                "mrs {hcr}, HCR_EL2",
                vttbr = out(reg) vttbr,
                vtcr = out(reg) vtcr,
                hcr = out(reg) hcr,
                options(nostack, preserves_flags),
            );
        }
        vttbr == ((u64::from(self.vmid) << registers::VTTBR_EL2_VMID_SHIFT) | self.root.get())
            && vtcr == address::capabilities().stage2_vtcr_el2()
            && hcr & registers::HCR_EL2_VM != 0
    }

    /// Optional opportunistic normal-RAM block granule. Device mappings stay 4 KiB.
    pub const fn normal_block_size() -> Option<u64> {
        Some(registers::STAGE2_LEVEL_SIZES_4K[1])
    }

    /// Installs a 2 MiB normal-memory block, or accepts an identical mapping.
    ///
    /// Returns false if the slot already contains a final-level table; callers
    /// must then use page mappings. Existing tables are never promoted or freed.
    ///
    /// # Safety
    /// The entire aligned physical interval must be retained, contiguous, and
    /// authorized with identical permissions. The hierarchy must be
    /// exclusively mutated; allocator obeys `new`'s contract. Before a running
    /// guest resumes, descriptor stores must be published.
    pub unsafe fn try_map_normal_block(
        &mut self,
        ipa: u64,
        physical: u64,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<bool, Error> {
        let size = registers::STAGE2_LEVEL_SIZES_4K[1];
        validate_range(ipa, physical, size)?;
        if !ipa.is_multiple_of(size) || !physical.is_multiple_of(size) {
            return Err(Error::InvalidRange);
        }
        let root_entry = read_entry(self.root, index(ipa, 0))?;
        if root_entry != 0 {
            if root_entry & registers::TRANSLATION_DESC_TYPE_MASK
                != registers::STAGE2_DESC_TABLE_OR_PAGE
            {
                return Err(Error::Conflict);
            }
            let l2 =
                PhysicalAddress::new(root_entry & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT);
            let existing = read_entry(l2, index(ipa, 1))?;
            if existing & registers::TRANSLATION_DESC_TYPE_MASK
                == registers::STAGE2_DESC_TABLE_OR_PAGE
            {
                return Ok(false);
            }
            if existing != 0 {
                let expected = leaf_descriptor(physical, 1, MemoryType::Normal, permissions);
                return if existing == expected {
                    Ok(true)
                } else {
                    Err(Error::Conflict)
                };
            }
        }
        self.map_leaf(ipa, physical, 1, MemoryType::Normal, permissions, allocator)?;
        Ok(true)
    }

    /// Installs or republishes an identical normal block for the active VMID.
    ///
    /// # Safety
    /// `try_map_normal_block`'s contracts apply and this address space must be
    /// selected on the current CPU throughout publication.
    pub unsafe fn try_map_normal_block_active(
        &mut self,
        ipa: u64,
        physical: u64,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<bool, ActiveMappingError<Error>> {
        // SAFETY: The caller supplies the same allocator and mapping authority.
        let mapped = unsafe { self.try_map_normal_block(ipa, physical, permissions, allocator) }
            .map_err(ActiveMappingError::BeforeInstall)?;
        if mapped {
            // SAFETY: Only an absent or identical block was accepted, so
            // publication cannot expose replacement backing or permissions.
            unsafe { publish_new_leaf() };
        }
        Ok(mapped)
    }

    /// Adds one normal-memory page mapping.
    ///
    /// # Safety
    ///
    /// Newly returned table pages must satisfy the ownership, initialization,
    /// mapping, alignment, and lifetime contract of [`Self::new`]. The caller
    /// must also serialize all updates to this hierarchy.
    pub unsafe fn map_normal_page(
        &mut self,
        ipa: u64,
        physical: u64,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_page(ipa, physical)?;
        self.map_leaf(ipa, physical, 2, MemoryType::Normal, permissions, allocator)
    }

    /// Adds a 4 KiB invalid-to-valid mapping while this VMID is active, then
    /// publishes that new leaf to the guest translation regime.
    ///
    /// # Safety
    ///
    /// This address space must be active on the current CPU, and the caller
    /// must serialize page-table updates for this VM. Newly returned table
    /// pages must also satisfy the ownership, mapping, and lifetime contract
    /// of [`Self::new`].
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
                // SAFETY: This method inherits the allocator and serialization
                // requirements in addition to requiring the hierarchy active.
                unsafe { stage2.map_normal_page(ipa, physical, permissions, allocator) }
            },
            |_| {
                // SAFETY: The caller guarantees this VMID remains active while
                // the new descriptor is published to the guest regime.
                unsafe { publish_new_leaf() };
                Ok(())
            },
        )
    }

    /// Grants execute permission to an existing inactive normal-memory page.
    ///
    /// Instruction bytes must already have been published to the instruction
    /// coherence domain. The next activation publishes this descriptor store.
    ///
    /// # Safety
    /// The caller excludes guest execution and serializes hierarchy mutation;
    /// newly allocated split tables satisfy `new`'s allocation contract.
    pub unsafe fn make_normal_page_executable(
        &mut self,
        ipa: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        if self.normal_leaf_is_executable(ipa)? {
            return Ok(());
        }
        // SAFETY: The caller supplies uniquely owned, retained table pages
        // and excludes concurrent mutation and guest execution.
        let (pointer, descriptor) = unsafe { self.ensure_page_leaf(ipa, allocator)? };
        // SAFETY: The validated leaf belongs to this exclusively mutated,
        // inactive hierarchy.
        unsafe { write_volatile(pointer, descriptor & !registers::STAGE2_DESC_XN) };
        Ok(())
    }

    /// Grants execute permission to an existing active normal-memory page.
    ///
    /// # Safety
    ///
    /// This address space must be active on the current CPU and serialized by
    /// the owning VM address-space lock. Invalidation is broadcast to all CPUs
    /// in the inner-shareable domain. Instruction bytes must have
    /// completed cache publication before this call. Newly allocated split
    /// tables must satisfy `new`'s allocation and lifetime contract.
    pub unsafe fn make_normal_page_executable_active(
        &mut self,
        ipa: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), ActiveMappingError<Error>> {
        if self
            .normal_leaf_is_executable(ipa)
            .map_err(ActiveMappingError::BeforeInstall)?
        {
            return Ok(());
        }
        // SAFETY: The caller supplies retained table pages and serializes
        // mutation. A failed split has not changed any translation; a successful
        // split returns the leaf directly, with no fallible post-commit walk.
        let (pointer, descriptor) = unsafe { self.ensure_page_leaf(ipa, allocator) }
            .map_err(ActiveMappingError::BeforeInstall)?;
        let executable = descriptor & !registers::STAGE2_DESC_XN;
        // Permission replacement uses break-before-make. All validation is
        // complete before the break, so no recoverable failure can strand the
        // live address space with an invalid descriptor.
        // SAFETY: The method contract guarantees exclusive mutation while the
        // exact VMID is active.
        unsafe { write_volatile(pointer, 0) };
        // SAFETY: The invalid descriptor is visible to the current guest
        // regime only after the architecture-mandated break invalidation.
        unsafe { invalidate_broken_ipa(ipa) };
        // SAFETY: The same validated leaf remains owned and fixed throughout
        // the IRQ-masked VM-exit transaction.
        unsafe { write_volatile(pointer, executable) };
        // SAFETY: Publish the new valid descriptor before ERET retries the
        // faulting instruction. ERET supplies the local context sync event.
        unsafe { publish_new_leaf() };
        Ok(())
    }

    /// Reissues new-leaf publication for one unchanged active guest page.
    ///
    /// # Safety
    ///
    /// This address space must be active on the current CPU.
    pub unsafe fn invalidate_page_active(&self, ipa: u64) -> Result<(), Error> {
        if ipa & (PAGE_SIZE - 1) != 0 || ipa >= address::STAGE2_IPA_LIMIT {
            return Err(Error::InvalidAddress);
        }
        // SAFETY: The method contract guarantees that this address space is
        // selected by the current CPU's VTTBR_EL2. Current callers use this
        // only as recovery after a fault on an unchanged valid leaf.
        unsafe { invalidate_existing_ipa(ipa) };
        Ok(())
    }

    /// Maps device memory using page-table pages supplied by `allocator`.
    ///
    /// # Safety
    ///
    /// Newly returned table pages must satisfy the ownership, initialization,
    /// mapping, alignment, and lifetime contract of [`Self::new`]. The caller
    /// must also serialize all updates to this hierarchy.
    pub unsafe fn map_device(
        &mut self,
        ipa: u64,
        physical: u64,
        size: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        self.map_range(ipa, physical, size, MemoryType::Device, allocator)
    }

    /// Installs this VM's stage-2 hierarchy on the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must exclusively own the local guest execution context and
    /// must not switch VMIDs without first stopping lower-EL execution.
    pub unsafe fn activate(&self) {
        let vttbr = (u64::from(self.vmid) << registers::VTTBR_EL2_VMID_SHIFT) | self.root.get();
        let vtcr = address::capabilities().stage2_vtcr_el2();
        // VHE makes TGE select whether guest or host TLBs are targeted. Keep
        // the temporary guest-regime interval entirely inside this register-
        // only sequence so host memory accesses cannot observe it.
        // SAFETY: The hierarchy is complete and owned by this address space.
        unsafe {
            asm!(
                "dsb ishst",
                "mrs {host_hcr}, HCR_EL2",
                "orr {host_hcr}, {host_hcr}, {vm}",
                "msr VTCR_EL2, {vtcr}",
                "msr VTTBR_EL2, {vttbr}",
                "msr HCR_EL2, {host_hcr}",
                "isb",
                "bic {guest_hcr}, {host_hcr}, {tge}",
                "msr HCR_EL2, {guest_hcr}",
                "isb",
                "tlbi VMALLS12E1IS",
                "dsb ish",
                "isb",
                "msr HCR_EL2, {host_hcr}",
                "isb",
                vtcr = in(reg) vtcr,
                vttbr = in(reg) vttbr,
                vm = in(reg) registers::HCR_EL2_VM,
                tge = in(reg) registers::HCR_EL2_TGE,
                host_hcr = out(reg) _,
                guest_hcr = out(reg) _,
                options(nostack, preserves_flags)
            );
        }
    }

    /// Prepares a revocation range without changing its translations or permissions.
    /// Only partially covered blocks require an L3 allocation. On failure any
    /// completed split is translation-equivalent, and no backing was revoked.
    /// The owning VM lock must remain held through the subsequent clear.
    ///
    /// # Safety
    /// The caller serializes all hierarchy changes, and newly allocated tables
    /// satisfy `new`'s ownership and lifetime contract.
    pub unsafe fn prepare_clear_normal_range(
        &mut self,
        ipa: u64,
        size: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_ipa_range(ipa, size)?;
        let end = ipa + size;
        let mut address = ipa;
        while address < end {
            if let Some((_, descriptor, level)) = self.leaf_at(address)? {
                validate_normal_leaf(descriptor, level)?;
                let span = registers::STAGE2_LEVEL_SIZES_4K[level];
                let base = address & !(span - 1);
                if level == 1 && (address != base || end - address < span) {
                    // SAFETY: The outer contract retains allocator pages and
                    // serializes this translation-equivalent split.
                    unsafe { self.ensure_page_leaf(address, allocator)? };
                    continue;
                }
                address = end.min(base + span);
            } else {
                address += PAGE_SIZE;
            }
        }
        Ok(())
    }

    /// Clears an already-prepared range while retaining backing and table owners.
    ///
    /// # Safety
    /// The caller must serialize prepare/clear, retain removed backing until
    /// all possible CPU consumers acknowledge subsequent live invalidation,
    /// and exclude new software lookup before clearing.
    pub unsafe fn clear_normal_range(&mut self, ipa: u64, size: u64) -> Result<(), Error> {
        validate_ipa_range(ipa, size)?;
        let end = ipa + size;
        // Validate the entire range before the first store, even if the caller
        // mistakenly omitted preparation. No partially cleared error result.
        for clear in [false, true] {
            let mut address = ipa;
            while address < end {
                if let Some((pointer, descriptor, level)) = self.leaf_at(address)? {
                    validate_normal_leaf(descriptor, level)?;
                    let span = registers::STAGE2_LEVEL_SIZES_4K[level];
                    if !address.is_multiple_of(span) || end - address < span {
                        return Err(Error::Conflict);
                    }
                    if clear {
                        // SAFETY: Exclusive mutation; backing remains retained
                        // through the caller's acknowledged invalidation.
                        unsafe { write_volatile(pointer, 0) };
                    }
                    address += span;
                } else {
                    address += PAGE_SIZE;
                }
            }
        }
        Ok(())
    }

    /// Returns the hardware leaf covering an IPA, including its level.
    fn leaf_at(&self, ipa: u64) -> Result<Option<(*mut u64, u64, usize)>, Error> {
        if !ipa.is_multiple_of(PAGE_SIZE) || ipa >= address::STAGE2_IPA_LIMIT {
            return Err(Error::InvalidAddress);
        }
        let mut table = self.root;
        for level in 0..3 {
            let pointer = table_pointer(table)?;
            // SAFETY: The hierarchy owns the validated table; the index is in
            // its 512-entry extent, and the VM owner serializes mutation.
            let pointer = unsafe { pointer.add(index(ipa, level)) };
            // SAFETY: The same retained aligned descriptor may be read by HW.
            let entry = unsafe { read_volatile(pointer) };
            if entry == 0 {
                return Ok(None);
            }
            let kind = entry & registers::TRANSLATION_DESC_TYPE_MASK;
            if (level == 1 && kind == registers::STAGE2_DESC_BLOCK)
                || (level == 2 && kind == registers::STAGE2_DESC_TABLE_OR_PAGE)
            {
                return Ok(Some((pointer, entry, level)));
            }
            if level == 2 || kind != registers::STAGE2_DESC_TABLE_OR_PAGE {
                return Err(Error::Conflict);
            }
            table = PhysicalAddress::new(entry & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT);
        }
        Err(Error::Conflict)
    }

    fn normal_leaf_is_executable(&self, ipa: u64) -> Result<bool, Error> {
        let (_, descriptor, level) = self.leaf_at(ipa)?.ok_or(Error::Conflict)?;
        validate_normal_leaf(descriptor, level)?;
        Ok(descriptor & registers::STAGE2_DESC_XN == 0)
    }

    /// Returns an existing page leaf or a translation-equivalent split leaf.
    /// Every error precedes the break; successful publication is followed only
    /// by returning pointers into the already-validated retained child.
    ///
    /// # Safety
    /// The caller exclusively mutates the hierarchy and supplies uniquely owned,
    /// zeroed table pages satisfying `new`'s retention/accessibility contract.
    unsafe fn ensure_page_leaf(
        &mut self,
        ipa: u64,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(*mut u64, u64), Error> {
        let (pointer, descriptor, level) = self.leaf_at(ipa)?.ok_or(Error::Conflict)?;
        validate_normal_leaf(descriptor, level)?;
        if level == 2 {
            return Ok((pointer, descriptor));
        }
        let child = allocator(1, 1).ok_or(Error::Allocation)?;
        let child_pointer = table_pointer(child)?;
        let physical = descriptor & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT;
        let attributes = descriptor
            & !registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT
            & !registers::TRANSLATION_DESC_TYPE_MASK;
        for slot in 0..registers::TRANSLATION_TABLE_ENTRY_COUNT_4K {
            let page = physical + slot as u64 * PAGE_SIZE;
            // SAFETY: A fresh zeroed exclusive table has exactly 512 entries;
            // each preserves the original block PA and all leaf attributes.
            unsafe {
                write_volatile(
                    child_pointer.add(slot),
                    page | attributes | registers::STAGE2_DESC_TABLE_OR_PAGE,
                )
            };
        }
        let slot = index(ipa, 2);
        // SAFETY: The initialized child is a retained 512-entry table and the
        // IPA-derived index lies within it. Resolve the result before commit.
        let page_pointer = unsafe { child_pointer.add(slot) };
        let page_descriptor = (physical + slot as u64 * PAGE_SIZE)
            | attributes
            | registers::STAGE2_DESC_TABLE_OR_PAGE;
        // All validation/allocation precedes break. From here there is no
        // fallible operation and every owner survives through VM retirement.
        // SAFETY: The VM lock serializes this parent slot and retains backing.
        unsafe { write_volatile(pointer, 0) };
        if self.vmid != 0 {
            self.invalidate_block_break();
        }
        // SAFETY: Either no hardware has ever selected this hierarchy, or the
        // synchronous break completed for the leased VMID on all shareable CPUs.
        unsafe { write_volatile(pointer, child.get() | registers::STAGE2_DESC_TABLE_OR_PAGE) };
        // SAFETY: Invalid-to-valid publication of the replacement table.
        unsafe { publish_new_leaf() };
        Ok((page_pointer, page_descriptor))
    }

    fn invalidate_block_break(&self) {
        let vttbr = (u64::from(self.vmid) << registers::VTTBR_EL2_VMID_SHIFT) | self.root.get();
        let vtcr = address::capabilities().stage2_vtcr_el2();
        // A block can have produced several cached combined translations. A
        // complete VMID invalidation avoids relying on a single IPA to cover
        // every subpage/translation size. Select the exact root even for an
        // inactive local VM; other CPUs may still execute or retain this VMID.
        // SAFETY: The caller exclusively owns the hierarchy and retains its
        // root/backing. Local IRQs are masked across this register-only interval
        // to prevent preemption under a temporary root. It restores the prior
        // selection; it neither schedules nor touches host memory with TGE off.
        unsafe {
            asm!(
                "mrs {saved_daif}, DAIF",
                "msr DAIFSet, #2",
                "dsb ishst",
                "mrs {saved_hcr}, HCR_EL2",
                "mrs {saved_vttbr}, VTTBR_EL2",
                "mrs {saved_vtcr}, VTCR_EL2",
                "orr {guest_hcr}, {saved_hcr}, {vm}",
                "bic {guest_hcr}, {guest_hcr}, {tge}",
                "msr VTCR_EL2, {vtcr}",
                "msr VTTBR_EL2, {vttbr}",
                "msr HCR_EL2, {guest_hcr}",
                "isb",
                "tlbi VMALLS12E1IS",
                "dsb ish",
                "isb",
                "msr VTTBR_EL2, {saved_vttbr}",
                "msr VTCR_EL2, {saved_vtcr}",
                "msr HCR_EL2, {saved_hcr}",
                "isb",
                "msr DAIF, {saved_daif}",
                vttbr = in(reg) vttbr,
                vtcr = in(reg) vtcr,
                vm = in(reg) registers::HCR_EL2_VM,
                tge = in(reg) registers::HCR_EL2_TGE,
                saved_daif = out(reg) _,
                saved_hcr = out(reg) _,
                saved_vttbr = out(reg) _,
                saved_vtcr = out(reg) _,
                guest_hcr = out(reg) _,
                options(nostack, preserves_flags)
            );
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn leaf_size_for_test(&self, ipa: u64) -> Result<Option<u64>, Error> {
        Ok(self
            .leaf_at(ipa)?
            .map(|(_, _, level)| registers::STAGE2_LEVEL_SIZES_4K[level]))
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn normal_leaf_for_test(
        &self,
        ipa: u64,
    ) -> Result<Option<(u64, u64, Stage2PagePermissions)>, Error> {
        self.leaf_at(ipa)?
            .map(|(_, descriptor, level)| {
                validate_normal_leaf(descriptor, level)?;
                let size = registers::STAGE2_LEVEL_SIZES_4K[level];
                let physical = (descriptor & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT)
                    + (ipa & (size - 1));
                let permissions = if descriptor & registers::STAGE2_DESC_XN == 0 {
                    Stage2PagePermissions::ReadWriteExecute
                } else {
                    Stage2PagePermissions::ReadWrite
                };
                Ok((physical, size, permissions))
            })
            .transpose()
    }

    fn map_range(
        &mut self,
        ipa: u64,
        physical: u64,
        size: u64,
        memory: MemoryType,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        validate_range(ipa, physical, size)?;

        let mut offset = 0;
        while offset < size {
            let current_ipa = ipa + offset;
            let current_physical = physical + offset;
            let level = 2;
            self.map_leaf(
                current_ipa,
                current_physical,
                level,
                memory,
                Stage2PagePermissions::ReadWriteExecute,
                allocator,
            )?;
            offset += registers::STAGE2_LEVEL_SIZES_4K[level];
        }
        Ok(())
    }

    fn map_leaf(
        &mut self,
        ipa: u64,
        physical: u64,
        leaf_level: usize,
        memory: MemoryType,
        permissions: Stage2PagePermissions,
        allocator: &mut impl FnMut(usize, usize) -> Option<PhysicalAddress>,
    ) -> Result<(), Error> {
        let mut table = self.root;
        for level in 0..leaf_level {
            let index = index(ipa, level);
            let entry = read_entry(table, index)?;
            table = if entry & registers::TRANSLATION_DESC_TYPE_MASK
                == registers::STAGE2_DESC_TABLE_OR_PAGE
            {
                PhysicalAddress::new(entry & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT)
            } else if entry == 0 {
                let child = allocator(1, 1).ok_or(Error::Allocation)?;
                validate_table(child)?;
                write_entry(
                    table,
                    index,
                    child.get() | registers::STAGE2_DESC_TABLE_OR_PAGE,
                )?;
                child
            } else {
                return Err(Error::Conflict);
            };
        }

        let descriptor = leaf_descriptor(physical, leaf_level, memory, permissions);
        let slot = index(ipa, leaf_level);
        let existing = read_entry(table, slot)?;
        if existing != 0 && existing != descriptor {
            return Err(Error::Conflict);
        }
        write_entry(table, slot, descriptor)
    }
}

fn leaf_descriptor(
    physical: u64,
    leaf_level: usize,
    memory: MemoryType,
    permissions: Stage2PagePermissions,
) -> u64 {
    let kind = if leaf_level == 2 {
        registers::STAGE2_DESC_TABLE_OR_PAGE
    } else {
        registers::STAGE2_DESC_BLOCK
    };
    let attributes = registers::STAGE2_DESC_ACCESS_FLAG
        | registers::STAGE2_DESC_READ_WRITE
        | match memory {
            MemoryType::Normal => normal_memory_attributes(permissions),
            MemoryType::Device => {
                registers::STAGE2_DESC_MEMATTR_DEVICE_NGNRE | registers::STAGE2_DESC_XN
            }
        };
    (physical & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT) | attributes | kind
}

fn validate_normal_leaf(descriptor: u64, level: usize) -> Result<(), Error> {
    const MEMATTR_MASK: u64 = 0xf << 2;
    if (level != 1 && level != 2)
        || descriptor & MEMATTR_MASK != registers::STAGE2_DESC_MEMATTR_NORMAL_WB
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

const fn normal_memory_attributes(permissions: Stage2PagePermissions) -> u64 {
    registers::STAGE2_DESC_INNER_SHAREABLE
        | registers::STAGE2_DESC_MEMATTR_NORMAL_WB
        | if permissions.is_executable() {
            0
        } else {
            registers::STAGE2_DESC_XN
        }
}

/// Invalidates a leased live guest translation identity while preserving selection.
pub(crate) fn synchronize_local(request: crate::arch::aarch64::GuestStage2RetirementRequest) {
    invalidate_local(request);
}

/// Parks a retired root without selecting its potentially reassigned VMID.
pub(crate) fn retire_root_local(root: u64) {
    // SAFETY: Global retirement excluded every execution owner of this root.
    // The register-only sequence preserves every unrelated selection.
    unsafe {
        asm!(
            "mrs {selected}, VTTBR_EL2",
            "and {physical}, {selected}, #0xffffffffffff",
            "cmp {physical}, {root}",
            "csel {selected}, xzr, {selected}, eq",
            "msr VTTBR_EL2, {selected}",
            "isb",
            root = in(reg) root,
            selected = out(reg) _, physical = out(reg) _,
            options(nostack),
        );
    }
    invalidate_namespace_local();
}

pub(crate) fn publish_changes() {
    // SAFETY: Publish descriptor stores before dispatching remote TLBI work.
    unsafe { asm!("dsb ishst", options(nostack)) };
}

fn invalidate_local(request: crate::arch::aarch64::GuestStage2RetirementRequest) {
    let retiring_vttbr = request.retiring_vttbr();
    let guest_vtcr = request.guest_vtcr();
    // SAFETY: The caller retains the exact root and VMID and executes with
    // local guest execution stopped. This register-only interval selects that
    // regime and performs a local combined stage-1/stage-2 invalidation. Live
    // synchronization restores all prior state; the lease pins its exact tag.
    unsafe {
        asm!(
            "mrs {saved_hcr}, HCR_EL2",
            "mrs {saved_vttbr}, VTTBR_EL2",
            "mrs {saved_vtcr}, VTCR_EL2",
            "orr {guest_hcr}, {saved_hcr}, {vm}",
            "bic {guest_hcr}, {guest_hcr}, {tge}",
            "msr VTCR_EL2, {guest_vtcr}",
            "msr VTTBR_EL2, {retiring_vttbr}",
            "msr HCR_EL2, {guest_hcr}",
            "isb",
            "dsb ishst",
            "tlbi VMALLS12E1",
            "dsb ish",
            "isb",
            "msr VTTBR_EL2, {saved_vttbr}",
            "msr VTCR_EL2, {saved_vtcr}",
            "msr HCR_EL2, {saved_hcr}",
            "isb",
            saved_hcr = out(reg) _,
            saved_vttbr = out(reg) _,
            saved_vtcr = out(reg) _,
            guest_hcr = out(reg) _,
            guest_vtcr = in(reg) guest_vtcr,
            retiring_vttbr = in(reg) retiring_vttbr,
            vm = in(reg) registers::HCR_EL2_VM,
            tge = in(reg) registers::HCR_EL2_TGE,
            options(nostack)
        );
    }
}

fn index(ipa: u64, level: usize) -> usize {
    ((ipa >> registers::STAGE2_LEVEL_SHIFTS_4K[level])
        & (registers::TRANSLATION_TABLE_ENTRY_COUNT_4K as u64 - 1)) as usize
}

fn validate_table(table: PhysicalAddress) -> Result<(), Error> {
    memory::linear_page_address(table)
        .map(|_| ())
        .ok_or(Error::InvalidAddress)
}

fn validate_page(ipa: u64, physical: u64) -> Result<(), Error> {
    if ipa & (PAGE_SIZE - 1) != 0 || physical & (PAGE_SIZE - 1) != 0 {
        return Err(Error::InvalidRange);
    }
    if ipa >= address::STAGE2_IPA_LIMIT
        || physical >= address::physical_address_limit()
        || ipa.checked_add(PAGE_SIZE).is_none()
        || physical.checked_add(PAGE_SIZE).is_none()
    {
        return Err(Error::InvalidAddress);
    }
    Ok(())
}

fn validate_range(ipa: u64, physical: u64, size: u64) -> Result<(), Error> {
    validate_ipa_range(ipa, size)?;
    if !physical.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidRange);
    }
    let physical_end = physical.checked_add(size).ok_or(Error::AddressOverflow)?;
    if physical_end > address::physical_address_limit() {
        return Err(Error::InvalidAddress);
    }
    Ok(())
}

fn validate_ipa_range(ipa: u64, size: u64) -> Result<(), Error> {
    if size == 0 || !ipa.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidRange);
    }
    let end = ipa.checked_add(size).ok_or(Error::AddressOverflow)?;
    if end > address::STAGE2_IPA_LIMIT {
        return Err(Error::InvalidAddress);
    }
    Ok(())
}

fn covering_regions(start: u64, end: u64, span: u64) -> Result<usize, Error> {
    let first = start / span;
    let last = end.checked_sub(1).ok_or(Error::InvalidRange)? / span;
    usize::try_from(last - first + 1).map_err(|_| Error::AddressOverflow)
}

/// Publishes one invalid-to-valid leaf for the active VMID.
///
/// Translation faults are not cached, so this path does not invalidate the
/// guest's complete stage-1 regime. Descriptor replacement and unmapping must
/// use [`invalidate_replaced_ipa`] instead.
unsafe fn publish_new_leaf() {
    // Translation faults are not cached, so invalid-to-valid publication only
    // has to make the descriptor store visible before guest retry. ERET is the
    // context-synchronization event for the faulting processing element.
    // SAFETY: DSB has no pointer operand and orders the serialized table write.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
}

/// Invalidates one unchanged valid leaf after an unexpected repeated fault.
unsafe fn invalidate_existing_ipa(ipa: u64) {
    let operand = ipa >> registers::TLBI_IPAS2E1_IPA_SHIFT;
    // SAFETY: The caller guarantees the current VTTBR_EL2 selects this address
    // space and keeps it active until the page-address invalidation completes.
    unsafe {
        asm!(
            "dsb ishst",
            "mrs {host_hcr}, HCR_EL2",
            "bic {guest_hcr}, {host_hcr}, {tge}",
            "msr HCR_EL2, {guest_hcr}",
            "isb",
            "tlbi ipas2e1is, {operand}",
            "dsb ish",
            "isb",
            "msr HCR_EL2, {host_hcr}",
            "isb",
            operand = in(reg) operand,
            tge = in(reg) registers::HCR_EL2_TGE,
            host_hcr = out(reg) _,
            guest_hcr = out(reg) _,
            options(nostack, preserves_flags)
        );
    }
}

/// Invalidates an existing stage-2 mapping and any guest stage-1 translations
/// which could have walked through its previous physical page.
///
/// This is intentionally separate from new-leaf publication: flushing the
/// complete guest stage-1 regime on every demand-zero fault would add needless
/// hot-path cost. Future replacement/unmap operations must call this helper
/// after publishing their descriptor update.
#[allow(dead_code)]
unsafe fn invalidate_replaced_ipa(ipa: u64) {
    let operand = ipa >> registers::TLBI_IPAS2E1_IPA_SHIFT;
    // SAFETY: The caller guarantees that VTTBR_EL2 selects the updated address
    // space. HCR.TGE is cleared while guest-regime TLBIs execute, then restored
    // only after both invalidations complete in the inner-shareable domain.
    unsafe {
        asm!(
            "dsb ishst",
            "mrs {host_hcr}, HCR_EL2",
            "bic {guest_hcr}, {host_hcr}, {tge}",
            "msr HCR_EL2, {guest_hcr}",
            "isb",
            "tlbi ipas2e1is, {operand}",
            "dsb ish",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            "msr HCR_EL2, {host_hcr}",
            "isb",
            operand = in(reg) operand,
            tge = in(reg) registers::HCR_EL2_TGE,
            host_hcr = out(reg) _,
            guest_hcr = out(reg) _,
            options(nostack, preserves_flags)
        );
    }
}

/// Completes the break phase of an active break-before-make update.
unsafe fn invalidate_broken_ipa(ipa: u64) {
    let operand = ipa >> registers::TLBI_IPAS2E1_IPA_SHIFT;
    // SAFETY: The old descriptor is already invalid, and the caller retains
    // exclusive mutation of the shared guest hierarchy. The first TLBI removes
    // stage-2 entries; VMALLE1IS also removes combined stage-1/stage-2 entries
    // which may cache the old execute denial.
    unsafe {
        asm!(
            "dsb ishst",
            "mrs {host_hcr}, HCR_EL2",
            "bic {guest_hcr}, {host_hcr}, {tge}",
            "msr HCR_EL2, {guest_hcr}",
            "isb",
            "tlbi ipas2e1is, {operand}",
            "dsb ish",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            "msr HCR_EL2, {host_hcr}",
            "isb",
            operand = in(reg) operand,
            tge = in(reg) registers::HCR_EL2_TGE,
            host_hcr = out(reg) _,
            guest_hcr = out(reg) _,
            options(nostack, preserves_flags)
        );
    }
}

fn read_entry(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = table_pointer(table)?;
    // SAFETY: The table is a live page owned by this stage-2 hierarchy.
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}

fn write_entry(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = table_pointer(table)?;
    // Hardware walkers on another CPU do not take the address-space lock.
    // Complete child-table initialization and backing-page writes before a
    // valid descriptor can expose them. The final publication barrier alone
    // would permit a walker to see this pointer ahead of those earlier stores.
    // SAFETY: DSB orders the owned normal-memory initialization before this
    // aligned descriptor store; the hierarchy owner serializes all writers.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    // SAFETY: The table allocation is retained and slot is a validated index.
    // Hardware reads aligned descriptors without taking Rust references.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}

fn table_pointer(table: PhysicalAddress) -> Result<*mut u64, Error> {
    memory::linear_page_address(table)
        .map(core::ptr::with_exposed_provenance_mut::<u64>)
        .ok_or(Error::InvalidAddress)
}

/// Flushes every guest VMID locally before admitting a new allocator epoch.
pub fn invalidate_namespace_local() {
    // SAFETY: EL2 owns the EL1/guest translation regime; ALLE1 covers every
    // VMID and leaves the VHE host regime intact. The caller keeps IRQs masked.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi ALLE1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        );
    }
}

/// Postorder traversal leaves the root and every nonempty subtree intact.
unsafe fn detach_empty(
    table: PhysicalAddress,
    level: usize,
    retire: &mut impl FnMut(PhysicalAddress),
) -> Result<bool, Error> {
    let pointer = table_pointer(table)?;
    let mut empty = true;
    for slot in 0..registers::TRANSLATION_TABLE_ENTRY_COUNT_4K {
        // SAFETY: Serialized access to this retained table's complete descriptor array.
        let entry = unsafe { pointer.add(slot) };
        // SAFETY: Hardware may read descriptors; all software writers are excluded.
        let descriptor = unsafe { read_volatile(entry) };
        if descriptor == 0 {
            continue;
        }
        if level < 2
            && descriptor & registers::TRANSLATION_DESC_TYPE_MASK
                == registers::STAGE2_DESC_TABLE_OR_PAGE
        {
            let child =
                PhysicalAddress::new(descriptor & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT);
            // SAFETY: This valid table descriptor names owned, retained child storage.
            if unsafe { detach_empty(child, level + 1, retire)? } {
                // SAFETY: Child storage remains owned until the caller's invalidation.
                unsafe { write_volatile(entry, 0) };
                retire(child);
                continue;
            }
        }
        empty = false;
    }
    Ok(empty)
}
