// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected host stage-1 memory capabilities.
//!
//! Kernel memory policy owns reservations, page-table lifecycle, stack slots,
//! and the synchronization that excludes live stage-1 mutation. This facade
//! hides the selected page-table format while retaining the ownership and
//! quiescence obligations of every unsafe operation. Cache maintenance and
//! atomic-backend diagnostics have independent selected facades.

#[cfg(CONFIG_CRASH_CONSOLE)]
use hyper::hal::memory::Stage1Mapping;
use hyper::hal::memory::{AddressTranslation, KernelImageLayout, VirtualMemoryLayout};
use hyper::mm::{BootAllocator, PhysicalAddress, VirtualAddress};
use hyper::platform::PlatformInfo;

/// Selected architectural ordering policy for physical device drivers.
pub type Barrier = crate::arch::memory::Barrier;

/// Failure reported by the selected stage-1 implementation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Error(crate::arch::memory::Error);

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error {
    const fn from_backend(error: crate::arch::memory::Error) -> Self {
        Self(error)
    }
}

/// Selected permanent stage-1 hierarchy retained by kernel boot state.
pub struct PreparedAddressSpace {
    backend: crate::arch::memory::PreparedAddressSpace,
    activation: Option<ActivationContext>,
}

/// Opaque input for the one-way permanent-address-space transition.
pub struct ActivationContext(crate::arch::memory::ActivationContext);

/// Opaque permanent-translation state copied into secondary CPU handoffs.
#[derive(Clone, Copy)]
pub struct SecondaryActivationContext(crate::arch::memory::SecondaryActivationContext);

/// One guarded runtime-stack mapping installed in the permanent hierarchy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackMapping {
    pub guard_page: usize,
    pub bottom: usize,
    pub top: usize,
}

impl StackMapping {
    const fn from_backend(mapping: crate::arch::memory::StackMapping) -> Self {
        Self {
            guard_page: mapping.guard_page,
            bottom: mapping.bottom,
            top: mapping.top,
        }
    }
}

impl PreparedAddressSpace {
    pub fn root_address(&self) -> u64 {
        self.backend.root_address()
    }

    pub fn kernel_base(&self) -> u64 {
        self.backend.kernel_base()
    }

    pub fn secondary_activation_context(&self) -> SecondaryActivationContext {
        SecondaryActivationContext(self.backend.secondary_activation_context())
    }

    /// Issues the one-shot transition token before this hierarchy is published.
    ///
    /// Requiring an exclusive borrow prevents issuance through immutable boot
    /// state, while taking the stored token makes duplicate issuance impossible.
    pub fn take_activation_context(&mut self) -> Option<ActivationContext> {
        self.activation.take()
    }

    /// Removes bootstrap aliases from the active stage-1 hierarchy.
    ///
    /// # Safety
    ///
    /// Every CPU must have abandoned the aliases being removed. The caller
    /// must exclusively serialize stage-1 mutation and keep the complete
    /// hierarchy alive and accessible throughout the operation.
    pub unsafe fn retire_identity_mappings(&self, platform: &PlatformInfo) -> Result<(), Error> {
        // SAFETY: The facade preserves the caller's CPU-quiescence, hierarchy
        // lifetime, and exclusive-mutation obligations.
        unsafe { self.backend.retire_identity_mappings(platform) }.map_err(Error::from_backend)
    }

    /// Installs one guarded kernel-stack mapping.
    ///
    /// # Safety
    ///
    /// `physical` must name `pages` uniquely owned, writable RAM pages that
    /// remain pinned while mapped. Every page returned by `allocate_table`
    /// must be a new, uniquely owned, zeroed, aligned, permanently accessible
    /// page retained with this hierarchy. The caller must serialize stage-1
    /// mutation.
    pub unsafe fn map_stack(
        &self,
        slot: usize,
        physical: PhysicalAddress,
        pages: usize,
        allocate_table: &mut dyn FnMut() -> Option<PhysicalAddress>,
    ) -> Result<StackMapping, Error> {
        // SAFETY: The facade forwards the complete backing-page, table-page,
        // lifetime, and serialized-mutation contract unchanged.
        unsafe {
            self.backend
                .map_stack(slot, physical, pages, allocate_table)
        }
        .map(StackMapping::from_backend)
        .map_err(Error::from_backend)
    }

    /// Removes one guarded kernel-stack mapping.
    ///
    /// # Safety
    ///
    /// No CPU, saved context, or unwinder may retain access to the mapping.
    /// The caller must exclusively serialize stage-1 mutation.
    pub unsafe fn unmap_stack(&self, slot: usize, pages: usize) -> Result<(), Error> {
        // SAFETY: The facade preserves the caller's quiescence and exclusive
        // stage-1 mutation guarantees.
        unsafe { self.backend.unmap_stack(slot, pages) }.map_err(Error::from_backend)
    }

    /// Tests whether a virtual address is present in this live hierarchy.
    ///
    /// # Safety
    ///
    /// The hierarchy must remain alive and accessible, and the caller must
    /// exclude concurrent page-table mutation for the complete walk.
    pub unsafe fn address_is_mapped(&self, address: usize) -> Result<bool, Error> {
        // SAFETY: The facade preserves hierarchy lifetime and walk exclusion.
        unsafe { self.backend.address_is_mapped(address) }.map_err(Error::from_backend)
    }
}

impl SecondaryActivationContext {
    pub(super) const fn into_backend(self) -> crate::arch::memory::SecondaryActivationContext {
        self.0
    }
}

pub fn bootstrap_accessible_limit() -> u64 {
    crate::arch::memory::AddressTranslation::bootstrap_accessible_limit()
}

pub fn virtual_memory_layout() -> VirtualMemoryLayout {
    crate::arch::memory::AddressTranslation::layout()
}

pub fn linear_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
    crate::arch::memory::AddressTranslation::linear_address(physical)
}

pub fn mmio_address(physical: PhysicalAddress) -> Option<VirtualAddress> {
    crate::arch::memory::AddressTranslation::mmio_address(physical)
}

/// Builds the permanent host stage-1 address space from boot-owned memory.
///
/// # Safety
///
/// Every page returned by `allocator` must be uniquely owned RAM writable
/// through the bootstrap mapping and must remain retained with the returned
/// hierarchy.
pub unsafe fn prepare(
    allocator: &mut BootAllocator,
    platform: &PlatformInfo,
    image: KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, Error> {
    // SAFETY: The selected backend receives the same allocator ownership,
    // accessibility, and hierarchy-lifetime contract.
    let backend = unsafe { crate::arch::memory::prepare(allocator, platform, image, kernel_base) }
        .map_err(Error::from_backend)?;
    let activation = ActivationContext(backend.activation_context());
    Ok(PreparedAddressSpace {
        backend,
        activation: Some(activation),
    })
}

/// Activates a prepared hierarchy and abandons the bootstrap call chain.
///
/// # Safety
///
/// `context` must come from a [`PreparedAddressSpace`] already retained in
/// global boot state. No live reference may depend on an alias removed by the
/// transition.
pub unsafe fn activate(context: ActivationContext) -> ! {
    // SAFETY: The opaque context can only be issued by this facade from a
    // selected hierarchy; the caller supplies retention and alias quiescence.
    unsafe { crate::arch::memory::activate(context.0) }
}

pub fn enable_local_protection() {
    crate::arch::memory::enable_local_protection();
}

pub fn local_protection_enabled() -> bool {
    crate::arch::memory::local_protection_enabled()
}

pub fn service_stage1_tlb_shootdown() -> bool {
    crate::arch::memory::service_stage1_tlb_shootdown()
}

#[cfg(all(CONFIG_ARCH_X86_64, feature = "kernel-self-test"))]
pub fn stage1_shootdown_count_for_test() -> u64 {
    crate::arch::memory::stage1_shootdown_count_for_test()
}

#[cfg(CONFIG_CRASH_CONSOLE)]
/// Walks an externally identified, live stage-1 hierarchy.
///
/// # Safety
///
/// `root` must identify the selected architecture's permanently retained,
/// aligned stage-1 root. All hierarchy pages must remain accessible and the
/// caller must exclude concurrent mutation throughout the walk.
pub unsafe fn inspect_stage1_mapping(
    root: u64,
    address: usize,
) -> Result<Option<Stage1Mapping>, Error> {
    // SAFETY: The facade forwards root validity, hierarchy lifetime, and walk
    // exclusion to the selected backend.
    unsafe { crate::arch::memory::inspect_stage1_mapping(root, address) }
        .map_err(Error::from_backend)
}
