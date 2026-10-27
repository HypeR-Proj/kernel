// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest-physical memory ownership and stage-2 demand paging.
//!
//! Construction, page publication, residency, retirement, and retained storage
//! have separate ownership protocols. This facade keeps their shared aggregate
//! and stable VM-facing API explicit without merging those protocols.

mod access;
pub(super) mod admission;
pub(super) mod backing;
mod construction;
mod extent_index;
mod grant_state;
pub(super) mod live;
pub(crate) mod live_service;
mod residency;
mod retirement;
mod storage;
mod synchronize;

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper::mm::ForeignCopyError;
use hyper::sync::atomic::AtomicU64;
use {
    crate::mm::allocator::BuddyError, hyper::mm::AddressSpaceResidency, hyper::mm::ResidencyError,
};

use crate::hal::vm::{Stage2AddressSpace, Stage2Error};
use crate::mm::user_space::MemoryObjectError;
use storage::{FixedBitmap, GuestMemoryBacking, Stage2PagePool};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceDomain,
    crate::accounting::ResourceError,
};

pub(crate) use access::resolve_guest_memory_fault;
pub(crate) use residency::{GuestResidencyClaim, activate, leave};
pub(crate) use retirement::{GuestStage2LocalRequest, service_local_retirement};
pub(crate) use synchronize::{SynchronizationError, prepare_live_synchronization};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Identifier(crate::mm::translation_id::Error),
    AddressOverflow,
    Allocation(BuddyError),
    Cache(hyper::hal::cache::CacheError),
    InvalidRange,
    MemoryObject,
    MetadataAllocation,
    InvalidCpu,
    Poisoned,
    TableMaintenance,
    Resource(ResourceError),
    Residency(ResidencyError),
    Registry(super::registry::Error),
    Stage2(Stage2Error),
}

impl From<BuddyError> for Error {
    fn from(error: BuddyError) -> Self {
        Self::Allocation(error)
    }
}

impl From<Stage2Error> for Error {
    fn from(error: Stage2Error) -> Self {
        Self::Stage2(error)
    }
}

impl From<hyper::hal::cache::CacheError> for Error {
    fn from(error: hyper::hal::cache::CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<super::registry::Error> for Error {
    fn from(error: super::registry::Error) -> Self {
        Self::Registry(error)
    }
}

impl From<MemoryObjectError> for Error {
    fn from(_error: MemoryObjectError) -> Self {
        Self::MemoryObject
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl From<ForeignCopyError<Error>> for Error {
    fn from(error: ForeignCopyError<Error>) -> Self {
        match error {
            ForeignCopyError::AddressOverflow => Self::AddressOverflow,
            ForeignCopyError::Backend(error) => error,
            ForeignCopyError::InvalidPageSize | ForeignCopyError::InvalidRange => {
                Self::InvalidRange
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestMemoryStats {
    pub addressable_pages: usize,
    pub committed_pages: usize,
    pub boot_committed_pages: usize,
    pub demand_faults: u64,
    pub read_faults: u64,
    pub write_faults: u64,
    pub execute_faults: u64,
    pub page_walk_faults: u64,
    pub repeated_faults: u64,
    pub failed_faults: u64,
}

/// VM-owned sparse guest RAM and its architecture stage-2 hierarchy.
impl GuestAddressSpace {
    pub(crate) fn dma_snapshot(&self) -> (backing::ResidentMemory, ResourceDomain) {
        (self.resident_memory(), self.domain.clone())
    }
}

pub(crate) struct GuestAddressSpace {
    ipa_base: u64,
    size: u64,
    domain: ResourceDomain,
    backing: GuestMemoryBacking,
    pub(super) live: live::LiveMappings,
    mapped_pages: FixedBitmap,
    instruction_ready_pages: FixedBitmap,
    committed_pages: usize,
    boot_committed_pages: usize,
    demand_faults: u64,
    read_faults: u64,
    write_faults: u64,
    execute_faults: u64,
    page_walk_faults: u64,
    repeated_faults: u64,
    failed_faults: u64,
    poisoned: bool,
    // Protected by the address-space lock. Detached parent slots must remain
    // invalid until every CPU completes the break-before-make invalidation.
    table_rebuild_blocked: bool,
    translation_epoch: u64,
    residency: AddressSpaceResidency<{ hyper::cpu::MAX_CPUS }>,
    instruction_epoch: AtomicU64,
    stage2: Stage2AddressSpace,
    table_pages: Stage2PagePool,
    identifier: Stage2Identifier,
    guest_page_charge: Option<CommittedCharge>,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    backing_page_charge: Option<CommittedCharge>,
    _metadata_charge: CommittedCharge,
}

pub(crate) type Stage2IdentifierReservation =
    crate::mm::translation_id::IdentifierReservation<crate::mm::translation_id::Stage2Vmid>;
type Stage2IdentifierLease =
    crate::mm::translation_id::IdentifierLease<crate::mm::translation_id::Stage2Vmid>;
type ActiveStage2Identifier =
    crate::mm::translation_id::ActiveIdentifier<crate::mm::translation_id::Stage2Vmid>;
type RetiringStage2Identifier =
    crate::mm::translation_id::RetiringIdentifier<crate::mm::translation_id::Stage2Vmid>;

enum Stage2Identifier {
    Reserved(Option<Stage2IdentifierReservation>),
    Active(ActiveStage2Identifier),
    Retiring(RetiringStage2Identifier),
    Retired,
    Poisoned,
}

impl Drop for GuestAddressSpace {
    fn drop(&mut self) {
        use super::address_space_state::{IdentifierState, destruction_is_safe};

        let state = match &self.identifier {
            Stage2Identifier::Reserved(_) => IdentifierState::Reserved,
            Stage2Identifier::Active(_) => IdentifierState::Active,
            Stage2Identifier::Retiring(_) => IdentifierState::Active,
            Stage2Identifier::Retired => IdentifierState::Retired,
            // Poisoned is installed only while consuming an unpublished VMID
            // reservation. A failed activation never published this address
            // space to hardware, so its pages remain safe to destroy.
            Stage2Identifier::Poisoned => IdentifierState::UnpublishedFailure,
        };
        if !destruction_is_safe(state) {
            // Drop runs before Rust destroys `backing`, `stage2`, and
            // `table_pages`. Fail closed here so active translation storage is
            // never returned while a CPU or stale TLB entry may reference it.
            hyper::debug::invariant_failure("vm::memory::drop invariant")
        }
    }
}
