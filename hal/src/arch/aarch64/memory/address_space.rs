// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::memory::KernelImageLayout;
#[cfg(CONFIG_CRASH_CONSOLE)]
use hyper::hal::memory::Stage1Mapping;
use hyper::mm::{BootAllocator, PhysicalAddress};
use hyper::platform::PlatformInfo;

use super::page_table::{self, FinalAddressSpace};

pub type Error = page_table::Error;

/// Architecture-owned state for the final EL2 stage-1 address space.
pub struct PreparedAddressSpace {
    address_space: FinalAddressSpace,
}

#[derive(Clone, Copy)]
pub struct ActivationContext {
    pub(in crate::arch::aarch64) transition_root: hyper::mm::PhysicalAddress,
    pub(in crate::arch::aarch64) kernel_root: hyper::mm::PhysicalAddress,
    pub(in crate::arch::aarch64) stack_top: hyper::mm::VirtualAddress,
    pub(in crate::arch::aarch64) kernel_base: u64,
    pub(in crate::arch::aarch64) tcr_el2: u64,
}

/// Inert permanent-translation state copied into a secondary boot handoff.
#[derive(Clone, Copy)]
pub struct SecondaryActivationContext {
    pub(in crate::arch::aarch64) transition_root: hyper::mm::PhysicalAddress,
    pub(in crate::arch::aarch64) kernel_root: hyper::mm::PhysicalAddress,
    pub(in crate::arch::aarch64) tcr_el2: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackMapping {
    pub guard_page: usize,
    pub bottom: usize,
    pub top: usize,
}

#[cfg(CONFIG_CRASH_CONSOLE)]
/// Walks an architecture stage-1 hierarchy captured by crash handling.
///
/// # Safety
///
/// `root` must name a live, aligned `AArch64` stage-1 root whose complete table
/// hierarchy remains readable through the permanent linear map. Page-table
/// updates must be excluded for the duration of the walk.
pub unsafe fn inspect_mapping(root: u64, address: usize) -> Result<Option<Stage1Mapping>, Error> {
    page_table::inspect_runtime_mapping(PhysicalAddress::new(root), address as u64)
}

impl PreparedAddressSpace {
    pub fn root_address(&self) -> u64 {
        self.address_space.kernel_root.get()
    }

    pub fn kernel_base(&self) -> u64 {
        self.address_space.kernel_base
    }

    pub fn activation_context(&self) -> ActivationContext {
        ActivationContext {
            transition_root: self.address_space.transition_root,
            kernel_root: self.address_space.kernel_root,
            stack_top: self.address_space.stack_top,
            kernel_base: self.address_space.kernel_base,
            tcr_el2: super::super::address::capabilities().stage1_tcr_el2(),
        }
    }

    pub fn secondary_activation_context(&self) -> SecondaryActivationContext {
        SecondaryActivationContext {
            transition_root: self.address_space.transition_root,
            kernel_root: self.address_space.kernel_root,
            tcr_el2: super::super::address::capabilities().stage1_tcr_el2(),
        }
    }

    /// Removes transition mappings from the active stage-1 hierarchy.
    ///
    /// # Safety
    ///
    /// Every participating CPU must already execute through permanent upper aliases,
    /// and all live references must use those aliases. No secondary may still
    /// enter through the transition hierarchy.
    /// The caller must exclusively serialize page-table changes and keep the
    /// hierarchy alive and accessible through the linear map.
    pub unsafe fn retire_identity_mappings(&self, _platform: &PlatformInfo) -> Result<(), Error> {
        // The shared HAL accepts platform intervals for architectures that mix
        // identity and permanent mappings. Here TTBR0 owns only transition
        // aliases, so retiring its entire root requires no interval walk.
        page_table::retire_identity_mappings(self.address_space.transition_root)
    }

    /// Adds a guarded runtime-stack mapping to this hierarchy.
    ///
    /// # Safety
    ///
    /// `physical` must identify `pages` uniquely owned, writable RAM pages
    /// that remain pinned while mapped. Every page returned by
    /// `allocate_table` must be a new, uniquely owned, writable, page-aligned
    /// page in the permanent linear map and remain alive with this hierarchy.
    /// The caller must serialize page-table updates.
    pub unsafe fn map_stack(
        &self,
        slot: usize,
        physical: PhysicalAddress,
        pages: usize,
        allocate_table: &mut dyn FnMut() -> Option<PhysicalAddress>,
    ) -> Result<StackMapping, Error> {
        page_table::map_runtime_stack(
            self.address_space.kernel_root,
            slot,
            physical,
            pages,
            allocate_table,
        )
    }

    /// Removes a runtime-stack mapping.
    ///
    /// # Safety
    ///
    /// No CPU, saved context, or unwinder may access the mapping, and the
    /// caller must exclusively serialize page-table changes.
    pub unsafe fn unmap_stack(&self, slot: usize, pages: usize) -> Result<(), Error> {
        page_table::unmap_runtime_stack(self.address_space.kernel_root, slot, pages)
    }

    /// Reads the live stage-1 hierarchy.
    ///
    /// # Safety
    ///
    /// The hierarchy must remain alive and mapped, and page-table mutation
    /// must be excluded throughout the walk.
    pub unsafe fn address_is_mapped(&self, address: usize) -> Result<bool, Error> {
        page_table::runtime_address_is_mapped(self.address_space.kernel_root, address as u64)
    }
}

/// Builds the `AArch64` final EL2 address space from kernel-owned boot memory.
///
/// # Safety
///
/// The allocator must return pages writable through the current bootstrap
/// identity mapping.
pub unsafe fn prepare(
    allocator: &mut BootAllocator,
    platform: &PlatformInfo,
    image: KernelImageLayout,
    kernel_base: u64,
) -> Result<PreparedAddressSpace, Error> {
    // SAFETY: This function forwards its bootstrap identity-map and allocator
    // accessibility contract to the page-table builder.
    let address_space =
        unsafe { page_table::build_final_address_space(allocator, platform, image, kernel_base)? };
    Ok(PreparedAddressSpace { address_space })
}
