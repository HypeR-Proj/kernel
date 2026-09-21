// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::memory::KernelImageLayout;
#[cfg(CONFIG_CRASH_CONSOLE)]
use hyper::hal::memory::Stage1Mapping;
use hyper::mm::{BootAllocator, PhysicalAddress};
use hyper::platform::PlatformInfo;

use super::page_table::{self, FinalAddressSpace};

pub type Error = page_table::Error;

pub struct PreparedAddressSpace {
    address_space: FinalAddressSpace,
}

#[derive(Clone, Copy)]
pub struct ActivationContext {
    pub(in crate::arch::riscv64) root: PhysicalAddress,
    pub(in crate::arch::riscv64) stack_top: hyper::mm::VirtualAddress,
    pub(in crate::arch::riscv64) kernel_base: u64,
}

/// Inert permanent-translation state copied into a secondary boot handoff.
#[derive(Clone, Copy)]
pub struct SecondaryActivationContext {
    pub(in crate::arch::riscv64) root: PhysicalAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackMapping {
    pub guard_page: usize,
    pub bottom: usize,
    pub top: usize,
}

#[cfg(CONFIG_CRASH_CONSOLE)]
/// Inspects a mapping rooted at an externally supplied page table.
///
/// # Safety
///
/// `root` must identify a live, well-formed HS page-table hierarchy whose
/// table pages remain accessible through the kernel linear mapping.
pub unsafe fn inspect_mapping(root: u64, address: usize) -> Result<Option<Stage1Mapping>, Error> {
    unsafe { page_table::inspect_runtime_mapping(PhysicalAddress::new(root), address as u64) }
}

impl PreparedAddressSpace {
    pub fn root_address(&self) -> u64 {
        self.address_space.root.get()
    }

    pub fn kernel_base(&self) -> u64 {
        self.address_space.kernel_base
    }

    pub fn activation_context(&self) -> ActivationContext {
        ActivationContext {
            root: self.address_space.root,
            stack_top: self.address_space.stack_top,
            kernel_base: self.address_space.kernel_base,
        }
    }

    pub fn secondary_activation_context(&self) -> SecondaryActivationContext {
        SecondaryActivationContext {
            root: self.address_space.root,
        }
    }

    /// Removes bootstrap identity mappings.
    ///
    /// # Safety
    ///
    /// The caller must serialize page-table mutation and ensure no CPU can
    /// concurrently walk or use the mappings being retired.
    pub unsafe fn retire_identity_mappings(&self, platform: &PlatformInfo) -> Result<(), Error> {
        // SAFETY: The caller supplies the required page-table and CPU quiescence.
        unsafe { page_table::retire_identity_mappings(self.address_space.root, platform) }
    }

    /// Maps a per-CPU kernel stack.
    ///
    /// # Safety
    ///
    /// Every page returned by `allocate_table` must be uniquely owned, zeroed,
    /// page-aligned RAM that remains live and linearly accessible while the
    /// address space uses it. The caller must serialize page-table mutation.
    pub unsafe fn map_stack(
        &self,
        slot: usize,
        physical: PhysicalAddress,
        pages: usize,
        allocate_table: &mut dyn FnMut() -> Option<PhysicalAddress>,
    ) -> Result<StackMapping, Error> {
        // SAFETY: The caller forwards allocator ownership and mutation serialization.
        unsafe {
            page_table::map_runtime_stack(
                self.address_space.root,
                slot,
                physical,
                pages,
                allocate_table,
            )
        }
    }

    /// Unmaps a per-CPU kernel stack.
    ///
    /// # Safety
    ///
    /// Page-table mutation must be serialized, and no CPU may retain or use a
    /// translation for the stack while it is being removed.
    pub unsafe fn unmap_stack(&self, slot: usize, pages: usize) -> Result<(), Error> {
        // SAFETY: The caller guarantees mutation serialization and stack quiescence.
        unsafe { page_table::unmap_runtime_stack(self.address_space.root, slot, pages) }
    }

    /// Reads the live page-table hierarchy.
    ///
    /// # Safety
    ///
    /// The caller must prevent concurrent mutation of the hierarchy.
    pub unsafe fn address_is_mapped(&self, address: usize) -> Result<bool, Error> {
        // SAFETY: The caller prevents concurrent hierarchy mutation.
        unsafe { page_table::runtime_address_is_mapped(self.address_space.root, address as u64) }
    }
}

/// Builds the permanent HS-mode address space while translation is disabled.
///
/// # Safety
///
/// Allocator results must be directly writable physical RAM.
pub unsafe fn prepare(
    allocator: &mut BootAllocator,
    platform: &PlatformInfo,
    image: KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, Error> {
    Ok(PreparedAddressSpace {
        // SAFETY: This function forwards its directly writable allocator contract.
        address_space: unsafe {
            page_table::build_final_address_space(allocator, platform, image, kernel_base)?
        },
    })
}
