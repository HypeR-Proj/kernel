// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::mm::{PhysicalAddress, VirtualAddress};

/// Failure reported before issuing cache maintenance operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheError {
    AddressOverflow,
    InvalidLineSize,
    NotInitialized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationLayoutError {
    AddressOverflow,
    EmptyPayload,
    InvalidAlignment,
    OutsideOwnedRange,
    VirtualAliasMisaligned,
}

/// A cache-publication range valid through physical and linear-map aliases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationLayout {
    physical_address: PhysicalAddress,
    virtual_address: VirtualAddress,
    published_size: usize,
}

impl PublicationLayout {
    /// Places one payload inside an exclusively owned aliased range.
    ///
    /// `physical_start` and `virtual_start` must identify the same first byte
    /// of a contiguous `owned_size` range. Physical alignment is established
    /// first for an external consumer of that address. The corresponding
    /// virtual alias is then independently validated before it can be used to
    /// construct a typed pointer or issue virtual-address cache maintenance.
    pub fn new(
        physical_start: PhysicalAddress,
        virtual_start: VirtualAddress,
        owned_size: usize,
        payload_size: usize,
        payload_alignment: usize,
        cache_line_size: usize,
    ) -> Result<Self, PublicationLayoutError> {
        let physical_start = physical_start.get();
        let virtual_start = virtual_start
            .as_usize()
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        if payload_size == 0 {
            return Err(PublicationLayoutError::EmptyPayload);
        }
        if payload_alignment == 0
            || !payload_alignment.is_power_of_two()
            || cache_line_size == 0
            || !cache_line_size.is_power_of_two()
        {
            return Err(PublicationLayoutError::InvalidAlignment);
        }
        let alignment = payload_alignment.max(cache_line_size);
        let physical_alignment =
            u64::try_from(alignment).map_err(|_| PublicationLayoutError::AddressOverflow)?;
        let owned_size_u64 =
            u64::try_from(owned_size).map_err(|_| PublicationLayoutError::AddressOverflow)?;
        physical_start
            .checked_add(owned_size_u64)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        virtual_start
            .checked_add(owned_size)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        let physical_address = align_up_u64(physical_start, physical_alignment)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        let offset = physical_address
            .checked_sub(physical_start)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        let virtual_address = virtual_start
            .checked_add(offset)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        if !virtual_address.is_multiple_of(alignment) {
            return Err(PublicationLayoutError::VirtualAliasMisaligned);
        }
        let published_size = align_up(payload_size, cache_line_size)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        let published_size_u64 =
            u64::try_from(published_size).map_err(|_| PublicationLayoutError::AddressOverflow)?;
        physical_address
            .checked_add(published_size_u64)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        virtual_address
            .checked_add(published_size)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        let published_end = offset
            .checked_add(published_size)
            .ok_or(PublicationLayoutError::AddressOverflow)?;
        if published_end > owned_size {
            return Err(PublicationLayoutError::OutsideOwnedRange);
        }
        let virtual_address =
            u64::try_from(virtual_address).map_err(|_| PublicationLayoutError::AddressOverflow)?;
        Ok(Self {
            physical_address: PhysicalAddress::new(physical_address),
            virtual_address: VirtualAddress::new(virtual_address),
            published_size,
        })
    }

    pub const fn physical_address(self) -> PhysicalAddress {
        self.physical_address
    }

    pub const fn virtual_address(self) -> VirtualAddress {
        self.virtual_address
    }

    pub const fn published_size(self) -> usize {
        self.published_size
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
}

fn align_up_u64(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
}

/// Tests whether cache-line rounding stays inside a page-granular owner.
///
/// Kernel-owned pages are aligned to `page_size`. A nonzero power-of-two line
/// no larger than that page therefore divides every page boundary, so rounding
/// a page-local range cannot touch an adjacent allocation.
pub const fn page_ownership_supports_line(line_size: usize, page_size: usize) -> bool {
    line_size != 0
        && page_size != 0
        && line_size.is_power_of_two()
        && page_size.is_power_of_two()
        && line_size <= page_size
}

/// Architecture policy for cache maintenance by virtual address.
pub trait CacheMaintenance {
    fn data_line_size() -> usize;
    fn instruction_line_size() -> usize;

    /// Publishes dirty data to the platform's coherent memory domain.
    ///
    /// # Safety
    ///
    /// The complete rounded cache-line range must be mapped and readable. The
    /// caller must own the buffer and prevent concurrent CPU writes until the
    /// receiving agent has taken ownership. This primitive does not wait for
    /// a device transaction or replace a direction-aware DMA API.
    unsafe fn publish_data_range(start: usize, length: usize) -> Result<(), CacheError>;

    /// Discards cached data before observing writes from another agent.
    ///
    /// # Safety
    ///
    /// The caller must exclusively own every rounded cache line, have already
    /// established that the producing agent completed its writes, and ensure
    /// that discarding dirty data cannot corrupt adjacent objects. A barrier
    /// cannot by itself establish device completion.
    unsafe fn discard_data_range(start: usize, length: usize) -> Result<(), CacheError>;

    /// Publishes dirty data and then discards cached copies of the range.
    ///
    /// # Safety
    ///
    /// The complete rounded cache-line range must be mapped and exclusively
    /// owned for the duration of the operation.
    unsafe fn publish_and_discard_data_range(start: usize, length: usize)
    -> Result<(), CacheError>;

    /// Publishes newly written instructions to the instruction-coherence
    /// domain, but does not synchronize another CPU's execution pipeline.
    ///
    /// # Safety
    ///
    /// The range must be mapped, writable before this call, and protected from
    /// concurrent execution or modification until synchronization completes.
    /// Every CPU that can subsequently execute the range must perform
    /// `synchronize_instruction_execution` after observing publication.
    unsafe fn publish_instruction_range(start: usize, length: usize) -> Result<(), CacheError>;

    /// Publishes a stable set of newly written instruction ranges as one
    /// cache-maintenance transaction.
    ///
    /// Architectures may enumerate `ranges` more than once so that all data
    /// lines can be cleaned before any instruction lines are invalidated. The
    /// enumerator must therefore yield the same ranges, in the same ownership
    /// state, on every invocation. A default implementation preserves the
    /// single-range behavior for architectures without a batched primitive.
    ///
    /// # Safety
    ///
    /// Every yielded range must satisfy [`Self::publish_instruction_range`]'s
    /// mapping, ownership, and execution-exclusion contract for the complete
    /// transaction, including every enumeration pass.
    unsafe fn publish_instruction_ranges(
        mut ranges: impl FnMut(&mut dyn FnMut(usize, usize)),
    ) -> Result<(), CacheError> {
        let mut result = Ok(());
        ranges(&mut |start, length| {
            if result.is_ok() {
                // SAFETY: The batch contract applies the single-range safety
                // requirements to every range yielded by the enumerator.
                result = unsafe { Self::publish_instruction_range(start, length) };
            }
        });
        result
    }

    /// Maintains instruction visibility for memory already owned by a guest.
    ///
    /// Unlike host code publication, this permits coherent guest writes while
    /// maintenance runs. It must never discard dirty data. It does not make
    /// concurrent instruction modification well-defined: the guest remains
    /// responsible for its architecture's code-publication and CPU rendezvous
    /// protocol. The faulting CPU must synchronize before retrying execution.
    ///
    /// # Safety
    ///
    /// The complete rounded range must be mapped normal coherent memory, owned
    /// by this guest and retained throughout the call. No host Rust reference
    /// may alias guest writes. Host writers and DMA are excluded. A stage-2
    /// execute denial must still exclude instruction fetches from this page
    /// until maintenance completes; guest data writes may continue.
    unsafe fn prepare_guest_instruction_range(
        start: usize,
        length: usize,
    ) -> Result<(), CacheError>;

    /// Performs the local context synchronization required before executing
    /// instructions published by another CPU.
    fn synchronize_instruction_execution();

    /// Repairs architecture-local guest instruction state after vCPU migration.
    ///
    /// This is distinct from host instruction publication. Every architecture
    /// must state its migration contract explicitly so a newly selected backend
    /// cannot silently inherit a no-op that is invalid for its instruction
    /// coherence model.
    fn synchronize_guest_instruction_migration();

    /// Invalidates instruction-cache entries throughout the kernel's
    /// instruction-coherence domain. Other CPUs still require a local context
    /// synchronization event before executing affected instructions.
    fn invalidate_instruction_all();
}
