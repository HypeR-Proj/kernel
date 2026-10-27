// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest page commitment, copying, fault resolution, and publication.

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use core::ptr::copy_nonoverlapping;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use core::ptr::write_bytes;

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use crate::mm::allocator::heap::PageOwner;
use hyper::mm::{ForeignMemory, PAGE_SIZE, PhysicalAddress};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper::mm::{copy_from_foreign, copy_to_foreign};
use hyper::sync::atomic::Ordering;
use {hyper_vm_models::exit::GuestMemoryFault, hyper_vm_models::exit::MemoryAccess};
use {
    hyper_vm_models::translation::ActiveMappingError,
    hyper_vm_models::translation::Stage2FaultResolution,
    hyper_vm_models::translation::Stage2PagePermissions,
    hyper_vm_models::translation::resolve_stage2_fault,
};

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use super::GuestMemoryStats;
use super::residency::publish_current_residency;
use super::storage::{
    FixedBitmap, GuestMemoryBacking, ResidentInstructionSnapshot, accumulate_charge,
    bitmap_storage_bytes, linear_address,
};
use super::{Error, GuestAddressSpace};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use crate::mm::page_block::PageBlock;
use {crate::accounting::ResourceAmount, crate::accounting::ResourceKind};

struct ResidentBlock {
    ipa: u64,
    physical: u64,
    first: usize,
    count: usize,
}

#[derive(Clone, Copy)]
enum LeafPublication<'pin> {
    Inactive,
    Active(&'pin dyn hyper::cpu::PinnedExecution),
}

impl LeafPublication<'_> {
    const fn is_active(self) -> bool {
        matches!(self, Self::Active(_))
    }
}

impl GuestAddressSpace {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn verify_resident_blocks_for_test() -> Result<(), &'static str> {
        use super::backing::{Layout, Region};
        use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
        use {
            crate::mm::user_space::GuestMemoryBacking as Lease, crate::mm::user_space::VmoObject,
        };

        let Some(size) = crate::hal::vm::Stage2AddressSpace::normal_block_size() else {
            return Ok(());
        };
        let domain = ResourceDomain::try_new_root(
            ResourceLimits::UNLIMITED.with(ResourceKind::GuestPages, 1),
        )
        .map_err(|_| "block domain")?;
        let vmo = VmoObject::try_new_contiguous(size, &domain).map_err(|_| "block VMO")?;
        let lease = Lease::try_from_vmo(&vmo).map_err(|_| "block lease")?;
        let mut layout = Layout::try_new(size, &domain).map_err(|_| "block layout")?;
        let mut region = Some(Region::new(0, 0, size, lease.clone()).map_err(|_| "block region")?);
        layout.insert(&mut region).map_err(|_| "block insert")?;
        let identifier =
            crate::mm::translation_id::reserve::<crate::mm::translation_id::Stage2Vmid>(
                crate::hal::vm::guest_translation_identifier_bits().map_err(|_| "VMID width")?,
            )
            .map_err(|_| "block VMID")?;
        let mut space = Self::from_vmo(identifier, 0x4000_0000, size, layout, &domain)
            .map_err(|_| "block address space")?;
        let candidate = space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWrite)
            .map_err(|_| "block inspect")?
            .ok_or("resident contiguous RAM rejected")?;
        if candidate.first != 0
            || candidate.count as u64 * PAGE_SIZE != size
            || !candidate.physical.is_multiple_of(size)
        {
            return Err("block candidate boundaries");
        }
        if space.reserve_block_pages(candidate.count).is_some() {
            return Err("block exceeded quota");
        }
        // The same production admission helper rejected the optional block,
        // but the original one-page operation still has its full allowance.
        let single = domain
            .reserve(space.guest_page_amount())
            .map_err(|_| "single-page quota lost")?;
        drop(single);
        if space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWriteExecute)
            .map_err(|_| "execute inspect")?
            .is_some()
        {
            return Err("unpublished execute block admitted");
        }
        space
            .publish_resident_instructions()
            .map_err(|_| "block instruction publication")?;
        if space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWriteExecute)
            .map_err(|_| "published inspect")?
            .is_none()
        {
            return Err("published executable block rejected");
        }
        space
            .instruction_ready_pages
            .set(0, false)
            .map_err(|_| "mixed bitmap")?;
        if space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWriteExecute)
            .map_err(|_| "mixed inspect")?
            .is_some()
        {
            return Err("mixed instruction authority coalesced");
        }
        space
            .instruction_ready_pages
            .set(0, true)
            .map_err(|_| "restore bitmap")?;
        space
            .mapped_pages
            .set(0, true)
            .map_err(|_| "mapped bitmap")?;
        if space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWriteExecute)
            .map_err(|_| "mapped inspect")?
            .is_some()
        {
            return Err("existing leaf coalesced");
        }
        space
            .mapped_pages
            .set(0, false)
            .map_err(|_| "restore mapped")?;
        drop(space);

        // Physically contiguous, identical-owner neighbors still do not give
        // one region authority over a block that straddles their boundary.
        let mut layout = Layout::try_new(size, &domain).map_err(|_| "split layout")?;
        for offset in [0, size / 2] {
            let mut region = Some(
                Region::new(offset, offset, size / 2, lease.clone()).map_err(|_| "split region")?,
            );
            layout.insert(&mut region).map_err(|_| "split insert")?;
        }
        let identifier =
            crate::mm::translation_id::reserve::<crate::mm::translation_id::Stage2Vmid>(
                crate::hal::vm::guest_translation_identifier_bits().map_err(|_| "VMID width")?,
            )
            .map_err(|_| "split VMID")?;
        let space = Self::from_vmo(identifier, 0x4000_0000, size, layout, &domain)
            .map_err(|_| "split address space")?;
        if space
            .resident_block_candidate(17, Stage2PagePermissions::ReadWrite)
            .map_err(|_| "split inspect")?
            .is_some()
        {
            return Err("region boundary coalesced");
        }
        Ok(())
    }

    /// Returns a raw pointer into retained test RAM, without creating a borrow.
    /// Dereferencing requires retaining this VM and using only atomic accesses.
    #[cfg(all(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
    ))]
    pub(crate) fn atomic_counter_for_test(
        &self,
        ipa: u64,
    ) -> Result<*const core::sync::atomic::AtomicU64, Error> {
        self.ensure_healthy()?;
        if ipa & 7 != 0 {
            return Err(Error::InvalidRange);
        }
        let index = self.page_index(ipa).ok_or(Error::InvalidRange)?;
        let physical = self.backing_physical_page(index)?;
        let address = linear_address(physical)?
            .checked_add((ipa & (PAGE_SIZE - 1)) as usize)
            .ok_or(Error::AddressOverflow)?;
        Ok(address as *const core::sync::atomic::AtomicU64)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn copy_from(&mut self, ipa: u64, destination: &mut [u8]) -> Result<(), Error> {
        self.ensure_healthy()?;
        copy_from_foreign(self, ipa, destination).map_err(Error::from)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn copy_to(&mut self, ipa: u64, source: &[u8]) -> Result<(), Error> {
        self.ensure_healthy()?;
        copy_to_foreign(self, ipa, source).map_err(Error::from)
    }

    pub fn finish_boot_loading(&mut self) {
        self.boot_committed_pages = self.committed_pages;
    }

    /// Publishes every resident shared-backing page as potentially executable.
    ///
    /// The userspace VMM defines executable guest ranges. Until range metadata
    /// becomes part of the VM construction ABI, sealing conservatively
    /// publishes all pages populated by the loader. Sparse holes remain
    /// untouched and are still committed on first guest access.
    pub(crate) fn publish_resident_instructions(&mut self) -> Result<(), Error> {
        self.ensure_healthy()?;
        // Snapshot before disabling preemption: allocation and address
        // resolution are fallible and may be proportional to guest RAM, while
        // the snapshot freezes the exact ranges required by a possibly
        // multi-pass HAL operation.
        // Pages may become resident afterwards, but cannot disappear or change
        // physical identity while this address space retains its backing.
        let snapshot = self.snapshot_resident_instruction_pages()?;
        let pin = crate::task::scheduler::preempt_disable().map_err(|_| Error::InvalidCpu)?;
        let result = self.publish_instruction_snapshot(&pin, &snapshot);
        drop(pin);
        result
    }

    fn snapshot_resident_instruction_pages(&self) -> Result<ResidentInstructionSnapshot, Error> {
        let metadata_bytes = bitmap_storage_bytes(self.mapped_pages.len())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(Error::MetadataAllocation)?;
        let metadata_charge = self
            .domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, metadata_bytes))?
            .commit();
        let mut pages = FixedBitmap::try_new(self.mapped_pages.len())?;
        for page_index in 0..self.mapped_pages.len() {
            let resident = self.backing_page_is_resident(page_index)?;
            if resident {
                // Resolve once before pinning so recoverable address failures
                // cannot occur after a cache-maintenance phase has begun.
                let _ = linear_address(self.backing_physical_page(page_index)?)?;
            }
            pages.set(page_index, resident)?;
        }
        Ok(ResidentInstructionSnapshot {
            pages,
            _metadata_charge: metadata_charge,
        })
    }

    fn publish_instruction_snapshot(
        &mut self,
        pin: &dyn hyper::cpu::PinnedExecution,
        snapshot: &ResidentInstructionSnapshot,
    ) -> Result<(), Error> {
        self.ensure_table_writable()?;
        if snapshot.pages.len() != self.instruction_ready_pages.len() {
            return Err(Error::InvalidRange);
        }
        self.residency
            .check_inactive(self.translation_epoch)
            .map_err(Error::Residency)?;
        // SAFETY: The typed pin keeps both maintenance phases on one CPU. A
        // PendingVirtualMachine is not runnable, direct VMO writers are excluded
        // by the hardware lease, and `snapshot` fixes membership. Every
        // selected physical page and its linear alias remain stable through VM
        // retirement. Failure to resolve a prevalidated address would therefore
        // prove memory-ownership corruption after maintenance has begun.
        let result = unsafe {
            crate::hal::cache::publish_instruction_ranges(pin, |visit| {
                for (page_index, resident) in snapshot.pages.iter().enumerate() {
                    if resident {
                        let address = match self
                            .backing_physical_page(page_index)
                            .and_then(linear_address)
                        {
                            Ok(address) => address,
                            Err(error) => crate::diagnostics::crash::fatal(format_args!(
                                "HypeR: resident guest instruction page changed during publication: {error:?}"
                            )),
                        };
                        visit(address, PAGE_SIZE as usize);
                    }
                }
            })
        };
        result.map_err(Error::from)?;
        let mut translation_changed = false;
        for (page_index, resident) in snapshot.pages.iter().enumerate() {
            if resident
                && self.mapped_pages.get(page_index).unwrap_or(false)
                && !self
                    .instruction_ready_pages
                    .get(page_index)
                    .unwrap_or(false)
            {
                let ipa = self.page_ipa(page_index)?;
                let mut allocate =
                    |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
                // SAFETY: The VM remains inactive; its table pool retains all
                // zeroed allocations and no hardware can race this mutation.
                unsafe {
                    self.stage2
                        .make_normal_page_executable(ipa, &mut allocate)?
                };
                translation_changed = true;
            }
        }
        if translation_changed {
            self.commit_translation_change(None);
        }
        let mut published = false;
        for (page_index, resident) in snapshot.pages.iter().enumerate() {
            if resident {
                self.instruction_ready_pages.set(page_index, true)?;
                published = true;
            }
        }
        if published {
            self.advance_instruction_epoch();
        }
        Ok(())
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn instruction_snapshot_for_test(
        &self,
    ) -> Result<ResidentInstructionSnapshot, Error> {
        self.snapshot_resident_instruction_pages()
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn publish_instruction_snapshot_for_test(
        &mut self,
        pin: &dyn hyper::cpu::PinnedExecution,
        snapshot: &ResidentInstructionSnapshot,
    ) -> Result<(), Error> {
        self.publish_instruction_snapshot(pin, snapshot)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn prepare_active_leaf_page_for_test(
        &mut self,
        page_index: usize,
        pin: &dyn hyper::cpu::PinnedExecution,
    ) -> Result<(), Error> {
        let physical = self.prepare_backing_page(page_index)?;
        self.publish_instruction_page(page_index, physical, pin)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn instruction_page_ready_for_test(&self, page_index: usize) -> bool {
        self.instruction_ready_pages
            .get(page_index)
            .unwrap_or(false)
    }

    pub(crate) fn resident_memory(&self) -> super::backing::ResidentMemory {
        match &self.backing {
            GuestMemoryBacking::SharedVmo(layout) => layout.resident_memory(),
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(pages) => super::backing::ResidentMemory::owned(
                pages.iter().filter(|page| page.is_some()).count() as u64 * PAGE_SIZE,
            ),
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub fn statistics(&self) -> GuestMemoryStats {
        GuestMemoryStats {
            addressable_pages: self.mapped_pages.len(),
            committed_pages: self.committed_pages,
            boot_committed_pages: self.boot_committed_pages,
            demand_faults: self.demand_faults,
            read_faults: self.read_faults,
            write_faults: self.write_faults,
            execute_faults: self.execute_faults,
            page_walk_faults: self.page_walk_faults,
            repeated_faults: self.repeated_faults,
            failed_faults: self.failed_faults,
        }
    }

    fn resolve_guest_memory_fault(
        &mut self,
        fault: GuestMemoryFault,
        pin: &dyn hyper::cpu::PinnedExecution,
    ) -> Result<bool, Error> {
        self.ensure_healthy()?;
        if self.table_rebuild_blocked {
            // Retry the instruction without creating tables in detached parent
            // slots. Re-entering the guest permits host IRQ delivery; do not
            // spin under this lock while the retiring CPU waits for IPIs.
            return Ok(true);
        }
        let Some(page_index) = self.page_index(fault.address().get()) else {
            return self.resolve_live_fault(fault);
        };
        self.demand_faults = self.demand_faults.saturating_add(1);
        match fault.access() {
            MemoryAccess::Read => self.read_faults = self.read_faults.saturating_add(1),
            MemoryAccess::Write => self.write_faults = self.write_faults.saturating_add(1),
            MemoryAccess::Execute => self.execute_faults = self.execute_faults.saturating_add(1),
        }
        if fault.during_guest_page_walk() {
            self.page_walk_faults = self.page_walk_faults.saturating_add(1);
        }
        match resolve_stage2_fault(
            self.mapped_pages.get(page_index).unwrap_or(false),
            self.instruction_ready_pages
                .get(page_index)
                .unwrap_or(false),
            fault.access(),
            fault.during_guest_page_walk(),
        ) {
            Stage2FaultResolution::PromoteExecute => {
                self.promote_active_page_to_executable(page_index, pin)?;
            }
            Stage2FaultResolution::Refresh => {
                self.repeated_faults = self.repeated_faults.saturating_add(1);
                let ipa = self.page_ipa(page_index)?;
                // SAFETY: Fault dispatch proves this VM's stage-2 is active on
                // the current CPU, and the address-space lock serializes
                // invalidation.
                unsafe { self.stage2.invalidate_page_active(ipa)? };
            }
            Stage2FaultResolution::Map(permissions) => {
                self.commit_page(page_index, LeafPublication::Active(pin), permissions)
                    .inspect_err(|_| {
                        self.failed_faults = self.failed_faults.saturating_add(1);
                    })?;
            }
        }
        Ok(true)
    }

    fn commit_page(
        &mut self,
        page_index: usize,
        publication: LeafPublication<'_>,
        requested_permissions: Stage2PagePermissions,
    ) -> Result<(), Error> {
        self.ensure_table_writable()?;
        if self.mapped_pages.get(page_index).is_none() {
            return Err(Error::InvalidRange);
        }
        if self.mapped_pages.get(page_index).unwrap_or(false) {
            return Ok(());
        }
        // A seal snapshot can publish a VMO-resident page before stage-2 has a
        // leaf for it. Preserve that already-earned execute authority when a
        // later data access installs the leaf.
        let permissions = if self
            .instruction_ready_pages
            .get(page_index)
            .unwrap_or(false)
        {
            Stage2PagePermissions::ReadWriteExecute
        } else {
            requested_permissions
        };
        let active_cpu = if publication.is_active() {
            let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
            self.residency
                .check_active(cpu.get(), self.translation_epoch)
                .map_err(Error::Residency)?;
            Some(cpu)
        } else {
            self.residency
                .check_inactive(self.translation_epoch)
                .map_err(Error::Residency)?;
            None
        };
        if let Some(cpu) = active_cpu
            && self.try_commit_resident_block(page_index, permissions, cpu)?
        {
            return Ok(());
        }
        let page_charge = self.domain.reserve(self.guest_page_amount())?;
        let physical = self.prepare_backing_page(page_index)?;
        if permissions.is_executable()
            && let LeafPublication::Active(pin) = publication
        {
            self.publish_instruction_page_bytes(physical, pin)?;
        }
        let ipa = self.page_ipa(page_index)?;
        let mapping = {
            let mut allocate_table =
                |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
            if publication.is_active() {
                // SAFETY: Called only from a lower-EL translation fault while this
                // VM is active, under the address-space lock.
                unsafe {
                    self.stage2.map_normal_page_active(
                        ipa,
                        physical.get(),
                        permissions,
                        &mut allocate_table,
                    )
                }
            } else {
                // SAFETY: Stage2PagePool preserves the allocation contract stated
                // at construction, and &mut self plus the owning VM's
                // address-space lock serializes all hierarchy mutation.
                let result = unsafe {
                    self.stage2.map_normal_page(
                        ipa,
                        physical.get(),
                        permissions,
                        &mut allocate_table,
                    )
                };
                result.map_err(ActiveMappingError::BeforeInstall)
            }
        };
        let committed_error = match mapping {
            Ok(()) => None,
            Err(ActiveMappingError::BeforeInstall(error)) => {
                return match self.table_pages.take_error() {
                    Some(error) => Err(error),
                    None => Err(error.into()),
                };
            }
            Err(ActiveMappingError::InstalledButInvalidationFailed(error)) => Some(error),
        };
        // Publication commits physical ownership even when the subsequent
        // invalidation fails. Store the owner before reporting that failure so
        // the live descriptor can never point at a page returned to the buddy.
        accumulate_charge(&mut self.guest_page_charge, page_charge.commit());
        self.mapped_pages.set(page_index, true)?;
        self.committed_pages += 1;
        self.commit_translation_change(active_cpu);
        if let Some(error) = committed_error {
            self.poisoned = true;
            // A failed publication leaves concurrent hardware state ambiguous.
            // Retain physical ownership above before global fail-stop.
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: committed stage-2 mapping invalidation failed: {error:?}"
            ));
        }
        if requested_permissions.is_executable() {
            self.mark_instruction_page_ready(page_index)?;
        }
        Ok(())
    }

    /// Inspect only: never populate memory or widen a retained RAM region.
    fn resident_block_candidate(
        &self,
        page_index: usize,
        permissions: Stage2PagePermissions,
    ) -> Result<Option<ResidentBlock>, Error> {
        let Some(size) = crate::hal::vm::Stage2AddressSpace::normal_block_size() else {
            return Ok(None);
        };
        let ipa = self.page_ipa(page_index)? & !(size - 1);
        let Some(offset) = ipa.checked_sub(self.ipa_base) else {
            return Ok(None);
        };
        #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
        let GuestMemoryBacking::SharedVmo(backing) = &self.backing;
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        let backing = match &self.backing {
            GuestMemoryBacking::SharedVmo(backing) => backing,
            GuestMemoryBacking::KernelOwned(_) => return Ok(None),
        };
        if !backing.covers_range(offset, size) {
            return Ok(None);
        }
        let Some(first) = self.page_index(ipa) else {
            return Ok(None);
        };
        let count = usize::try_from(size / PAGE_SIZE).map_err(|_| Error::InvalidRange)?;
        let end = first.checked_add(count).ok_or(Error::InvalidRange)?;
        let executable = self.instruction_ready_pages.get(first).unwrap_or(false);
        // An execute fault only earns execute authority for its own page. A
        // block may execute only when every page was already cache-published.
        if permissions.is_executable() && !executable {
            return Ok(None);
        }
        // Once any page falls back to an L3 leaf this cheap check prevents
        // repeatedly scanning the backing of the same unsuitable block.
        for index in first..end {
            if self.mapped_pages.get(index) != Some(false)
                || self.instruction_ready_pages.get(index) != Some(executable)
            {
                return Ok(None);
            }
        }
        if !backing.page_is_resident(offset)? {
            return Ok(None);
        }
        let physical = backing.physical_page(offset)?.get();
        if !physical.is_multiple_of(size) {
            return Ok(None);
        }
        for index in 1..count {
            let delta = index as u64 * PAGE_SIZE;
            if !backing.page_is_resident(offset + delta)?
                || backing.physical_page(offset + delta)?.get() != physical + delta
            {
                return Ok(None);
            }
        }
        Ok(Some(ResidentBlock {
            ipa,
            physical,
            first,
            count,
        }))
    }

    /// Map an already-resident, uniformly published RAM block without changing
    /// backing allocation policy. A missed optimization must not make a
    /// previously admissible single-page fault fail its resource quota.
    fn try_commit_resident_block(
        &mut self,
        page_index: usize,
        permissions: Stage2PagePermissions,
        cpu: hyper::cpu::CpuIndex,
    ) -> Result<bool, Error> {
        let Some(ResidentBlock {
            ipa,
            physical,
            first,
            count,
        }) = self.resident_block_candidate(page_index, permissions)?
        else {
            return Ok(false);
        };
        let end = first + count;
        let Some(charge) = self.reserve_block_pages(count) else {
            return Ok(false);
        };
        let result = {
            let mut allocate =
                |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
            // SAFETY: The active residency check in commit_page and VM lock
            // serialize publication. One retained hardware-write lease covers
            // all stable, resident, contiguous pages; no authority is widened.
            unsafe {
                self.stage2
                    .try_map_normal_block_active(ipa, physical, permissions, &mut allocate)
            }
        };
        let committed_error = match result {
            Ok(false) => return Ok(false),
            Ok(true) => None,
            Err(ActiveMappingError::BeforeInstall(crate::hal::vm::Stage2Error::Allocation)) => {
                // A block is optional, including its table allocations. Any
                // successfully allocated empty tables remain owned by the pool.
                let _ = self.table_pages.take_error();
                return Ok(false);
            }
            Err(ActiveMappingError::BeforeInstall(error)) => return Err(error.into()),
            Err(ActiveMappingError::InstalledButInvalidationFailed(error)) => Some(error),
        };
        accumulate_charge(&mut self.guest_page_charge, charge.commit());
        for index in first..end {
            self.mapped_pages.set(index, true)?;
        }
        self.committed_pages += count;
        self.commit_translation_change(Some(cpu));
        if let Some(error) = committed_error {
            self.poisoned = true;
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: committed stage-2 block invalidation failed: {error:?}"
            ));
        }
        Ok(true)
    }

    fn reserve_block_pages(&self, count: usize) -> Option<crate::accounting::ChargeReservation> {
        self.domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::PinnedPages, count as u64)
                    .with(ResourceKind::GuestPages, count as u64),
            )
            .ok()
    }

    fn promote_active_page_to_executable(
        &mut self,
        page_index: usize,
        pin: &dyn hyper::cpu::PinnedExecution,
    ) -> Result<(), Error> {
        self.ensure_table_writable()?;
        let physical = self.backing_physical_page(page_index)?;
        self.publish_instruction_page_bytes(physical, pin)?;
        let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
        self.residency
            .check_active(cpu.get(), self.translation_epoch)
            .map_err(Error::Residency)?;
        let ipa = self.page_ipa(page_index)?;
        let mut allocate = |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
        // SAFETY: Fault dispatch proves this exact address space is active on
        // `cpu`, and the address-space lock serializes the
        // break-before-make/permission update.
        let promotion = unsafe {
            self.stage2
                .make_normal_page_executable_active(ipa, &mut allocate)
        };
        match promotion {
            Ok(()) => {}
            Err(ActiveMappingError::BeforeInstall(error)) => return Err(error.into()),
            Err(ActiveMappingError::InstalledButInvalidationFailed(error)) => {
                self.poisoned = true;
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: executable stage-2 permission invalidation failed: {error:?}"
                ));
            }
        }
        self.commit_translation_change(Some(cpu));
        self.mark_instruction_page_ready(page_index)
    }

    /// Publishes host-written bytes before an active executable stage-2 leaf
    /// can name this page. Data-only demand pages intentionally skip this path.
    fn publish_instruction_page_bytes(
        &self,
        physical: PhysicalAddress,
        pin: &dyn hyper::cpu::PinnedExecution,
    ) -> Result<(), Error> {
        let address = linear_address(physical)?;
        // SAFETY: This page is still stage-2 execute-denied (or unmapped).
        // The VM lease excludes host writers and retains the whole aligned
        // allocation. Sibling vCPUs may write data; the guest-specific helper
        // cleans without discarding dirty contents and does not claim to
        // synchronize guest code modifications. Guest software owns that
        // protocol. The faulting vCPU's ERET supplies local context sync.
        unsafe {
            crate::hal::cache::prepare_guest_instruction_range(pin, address, PAGE_SIZE as usize)
        }?;
        Ok(())
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    fn publish_instruction_page(
        &mut self,
        page_index: usize,
        physical: PhysicalAddress,
        pin: &dyn hyper::cpu::PinnedExecution,
    ) -> Result<(), Error> {
        self.publish_instruction_page_bytes(physical, pin)?;
        self.mark_instruction_page_ready(page_index)
    }

    fn mark_instruction_page_ready(&mut self, page_index: usize) -> Result<(), Error> {
        let ready = self
            .instruction_ready_pages
            .get(page_index)
            .ok_or(Error::InvalidRange)?;
        if !ready {
            self.instruction_ready_pages.set(page_index, true)?;
            self.advance_instruction_epoch();
        }
        Ok(())
    }

    fn commit_translation_change(&mut self, active_cpu: Option<hyper::cpu::CpuIndex>) {
        let previous_epoch = self.translation_epoch;
        self.translation_epoch = match previous_epoch.checked_add(1) {
            Some(epoch) => epoch,
            None => {
                self.poisoned = true;
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: stage-2 mapping epoch exhausted"
                ));
            }
        };
        if let Some(cpu) = active_cpu {
            if self
                .residency
                .advance_shared_active(cpu.get(), previous_epoch, self.translation_epoch)
                .is_err()
            {
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: active guest residency epoch publication is inconsistent"
                ));
            }
            let incarnation = match self.incarnation() {
                Ok(incarnation) => incarnation,
                Err(error) => crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: active guest mapping lost its VMID incarnation: {error:?}"
                )),
            };
            publish_current_residency(incarnation);
        } else if self
            .residency
            .advance_inactive(previous_epoch, self.translation_epoch)
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: inactive guest residency epoch publication is inconsistent"
            ));
        }
    }

    fn advance_instruction_epoch(&self) {
        if self
            .instruction_epoch
            .fetch_update(Ordering::Release, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            })
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: guest instruction publication epoch exhausted"
            ));
        }
    }

    fn prepare_backing_page(&mut self, page_index: usize) -> Result<PhysicalAddress, Error> {
        let offset = self.backing_offset(page_index)?;
        match &mut self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(pages) => {
                let slot = pages.get_mut(page_index).ok_or(Error::InvalidRange)?;
                if slot.is_none() {
                    let reservation = self.domain.reserve(
                        ResourceAmount::ZERO
                            .with(ResourceKind::KernelMemoryBytes, PAGE_SIZE)
                            .with(ResourceKind::CommittedPages, 1),
                    )?;
                    let page = PageBlock::allocate_for(0, PageOwner::Guest)?;
                    let physical = page.physical();
                    let virtual_address = linear_address(physical)?;
                    // SAFETY: The new page is exclusively VM-owned and fully
                    // covered by the permanent writable linear map.
                    unsafe { write_bytes(virtual_address as *mut u8, 0, PAGE_SIZE as usize) };
                    *slot = Some(page);
                    accumulate_charge(&mut self.backing_page_charge, reservation.commit());
                }
                slot.as_ref()
                    .map(PageBlock::physical)
                    .ok_or(Error::InvalidRange)
            }
            GuestMemoryBacking::SharedVmo(backing) => {
                backing.populate_page(offset)?;
                backing.physical_page(offset).map_err(Into::into)
            }
        }
    }

    fn guest_page_amount(&self) -> ResourceAmount {
        ResourceAmount::ZERO
            .with(ResourceKind::PinnedPages, 1)
            .with(ResourceKind::GuestPages, 1)
    }

    fn backing_page_is_resident(&self, page_index: usize) -> Result<bool, Error> {
        let offset = self.backing_offset(page_index)?;
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(pages) => pages
                .get(page_index)
                .map(Option::is_some)
                .ok_or(Error::InvalidRange),
            GuestMemoryBacking::SharedVmo(backing) => {
                backing.page_is_resident(offset).map_err(Into::into)
            }
        }
    }

    fn backing_physical_page(&self, page_index: usize) -> Result<PhysicalAddress, Error> {
        let offset = self.backing_offset(page_index)?;
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(pages) => pages
                .get(page_index)
                .and_then(Option::as_ref)
                .map(PageBlock::physical)
                .ok_or(Error::InvalidRange),
            GuestMemoryBacking::SharedVmo(backing) => {
                backing.physical_page(offset).map_err(Into::into)
            }
        }
    }

    pub(super) fn page_index(&self, address: u64) -> Option<usize> {
        let offset = address.checked_sub(self.ipa_base)?;
        if offset >= self.size {
            return None;
        }
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(_) => usize::try_from(offset / PAGE_SIZE).ok(),
            GuestMemoryBacking::SharedVmo(backing) => backing.page_index(offset),
        }
    }

    fn backing_offset(&self, page_index: usize) -> Result<u64, Error> {
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(_) => (page_index as u64)
                .checked_mul(PAGE_SIZE)
                .ok_or(Error::AddressOverflow),
            GuestMemoryBacking::SharedVmo(backing) => {
                backing.page_offset(page_index).ok_or(Error::InvalidRange)
            }
        }
    }

    fn page_ipa(&self, page_index: usize) -> Result<u64, Error> {
        self.ipa_base
            .checked_add(self.backing_offset(page_index)?)
            .ok_or(Error::AddressOverflow)
    }

    pub(super) fn ensure_healthy(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    pub(super) fn ensure_table_writable(&self) -> Result<(), Error> {
        self.ensure_healthy()?;
        if self.table_rebuild_blocked {
            Err(Error::TableMaintenance)
        } else {
            Ok(())
        }
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn verify_table_maintenance_for_test(&mut self) -> Result<(), &'static str> {
        let tables = self.table_pages.pages.len();
        let committed = self.committed_pages;
        let pin = crate::task::scheduler::preempt_disable().map_err(|_| "table maintenance pin")?;
        self.table_rebuild_blocked = true;
        let fault = GuestMemoryFault::new(
            hyper_vm_models::exit::GuestPhysicalAddress::new(self.ipa_base),
            MemoryAccess::Write,
            false,
        );
        let retry = self.resolve_guest_memory_fault(fault, &pin);
        let write = self.commit_page(
            0,
            LeafPublication::Inactive,
            Stage2PagePermissions::ReadWrite,
        );
        self.table_rebuild_blocked = false;
        if retry != Ok(true)
            || write != Err(Error::TableMaintenance)
            || self.table_pages.pages.len() != tables
            || self.committed_pages != committed
        {
            return Err("table maintenance allowed a replacement before invalidation");
        }
        Ok(())
    }
}

impl ForeignMemory for GuestAddressSpace {
    type Error = Error;

    fn address_base(&self) -> u64 {
        self.ipa_base
    }

    fn address_size(&self) -> u64 {
        self.size
    }

    fn page_size(&self) -> usize {
        PAGE_SIZE as usize
    }

    fn read_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error> {
        let linear_offset = (page_index as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(Error::AddressOverflow)?;
        let address = self
            .ipa_base
            .checked_add(linear_offset)
            .ok_or(Error::AddressOverflow)?;
        let page_index = self.page_index(address).ok_or(Error::InvalidRange)?;

        if !self.backing_page_is_resident(page_index)? {
            destination.fill(0);
            return Ok(());
        }
        let object_offset = linear_offset
            .checked_add(page_offset as u64)
            .ok_or(Error::AddressOverflow)?;
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(_) => {
                let physical = self.backing_physical_page(page_index)?;
                let source = linear_address(physical)?
                    .checked_add(page_offset)
                    .ok_or(Error::AddressOverflow)?;
                // SAFETY: The generic copy layer bounds this chunk to one
                // VM-owned page, whose mapping remains stable under the
                // address-space lock.
                unsafe {
                    copy_nonoverlapping(
                        source as *const u8,
                        destination.as_mut_ptr(),
                        destination.len(),
                    )
                };
                Ok(())
            }
            GuestMemoryBacking::SharedVmo(backing) => backing
                .read_exposed(object_offset, destination)
                .map_err(Into::into),
        }
    }

    fn write_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error> {
        let linear_offset = (page_index as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(Error::AddressOverflow)?;
        let address = self
            .ipa_base
            .checked_add(linear_offset)
            .ok_or(Error::AddressOverflow)?;
        let page_index = self.page_index(address).ok_or(Error::InvalidRange)?;

        self.commit_page(
            page_index,
            LeafPublication::Inactive,
            Stage2PagePermissions::ReadWrite,
        )?;
        let object_offset = linear_offset
            .checked_add(page_offset as u64)
            .ok_or(Error::AddressOverflow)?;
        match &self.backing {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            GuestMemoryBacking::KernelOwned(_) => {
                let physical = self.backing_physical_page(page_index)?;
                let destination = linear_address(physical)?
                    .checked_add(page_offset)
                    .ok_or(Error::AddressOverflow)?;
                // SAFETY: The generic copy layer bounds this chunk to one
                // VM-owned page, whose mapping remains stable under the
                // address-space lock.
                unsafe {
                    copy_nonoverlapping(source.as_ptr(), destination as *mut u8, source.len())
                };
                Ok(())
            }
            GuestMemoryBacking::SharedVmo(backing) => backing
                .write_exposed(object_offset, source)
                .map_err(Into::into),
        }
    }
}

pub(crate) fn resolve_guest_memory_fault(
    vm: &crate::vm::registry::VmBinding,
    fault: GuestMemoryFault,
    pin: &dyn hyper::cpu::PinnedExecution,
) -> Result<bool, Error> {
    vm.with_address_space(|address_space| address_space.resolve_guest_memory_fault(fault, pin))
}
