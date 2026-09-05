// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent memory-management mechanisms.

mod access;
mod address;
mod address_space_state;
mod allocation;
pub mod allocator;
pub mod boot;
pub mod kaslr;
mod translation_id;

pub use access::{ForeignCopyError, ForeignMemory, copy_from_foreign, copy_to_foreign};
pub use address::{PAGE_SIZE, PhysicalAddress, VirtualAddress};
pub use address_space_state::{
    AddressSpaceResidency, CutFailure, ResidencyError, RetirementCut, UpdateCut,
};
pub use allocation::{
    AllocationError, DeferredArcDrop, FallibleArc, UniqueFallibleArc, WeakFallibleArc, try_box,
};
pub use allocator::{BuddyAllocator, BuddyError, BuddyStats, MAX_ORDER, MemoryHandoff};
pub use boot::{BootAllocator, BootAllocatorError, BootMemoryStats};
pub use translation_id::{
    ActiveTranslationId, ReservedTranslationId, RetiringTranslationId, TranslationIdError,
    TranslationIdPool,
};
