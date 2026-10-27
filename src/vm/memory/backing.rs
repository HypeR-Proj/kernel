// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable, bounded guest RAM composition over explicitly retained VMO leases.

use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {crate::mm::user_space::GuestMemoryBacking, crate::mm::user_space::MemoryObjectError};

#[derive(Clone)]
pub(crate) struct Region {
    offset: u64,
    source_offset: u64,
    length: u64,
    backing: GuestMemoryBacking,
}

impl Region {
    pub(crate) fn new(
        offset: u64,
        source_offset: u64,
        length: u64,
        backing: GuestMemoryBacking,
    ) -> Result<Self, MemoryObjectError> {
        if length == 0
            || !offset.is_multiple_of(PAGE_SIZE)
            || !source_offset.is_multiple_of(PAGE_SIZE)
            || !length.is_multiple_of(PAGE_SIZE)
            || offset.checked_add(length).is_none()
            || source_offset
                .checked_add(length)
                .is_none_or(|end| end > backing.size())
        {
            return Err(MemoryObjectError::AllocationSize);
        }
        Ok(Self {
            offset,
            source_offset,
            length,
            backing,
        })
    }
}

/// Retains only primary backing leases while a normal-context inspector counts pages.
/// No VM address-space or lifecycle lock is held during the VMO scans.
pub(crate) struct ResidentMemory {
    regions: [Option<Region>; 8],
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    owned_bytes: u64,
}

impl ResidentMemory {
    pub(crate) fn dma_leases(
        &self,
        domain: &ResourceDomain,
    ) -> Result<[Option<crate::device::dma::Lease>; 8], MemoryObjectError> {
        let mut leases = [const { None }; 8];
        for (index, region) in self.regions.iter().enumerate() {
            if let Some(region) = region {
                leases[index] = Some(crate::device::dma::BackendMemoryLease::prepare_range(
                    region.backing.clone(),
                    region.source_offset,
                    region.length,
                    domain,
                )?);
            }
        }
        Ok(leases)
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn owned(bytes: u64) -> Self {
        Self {
            regions: [const { None }; 8],
            owned_bytes: bytes,
        }
    }

    pub(crate) fn bytes(&self) -> Result<u64, MemoryObjectError> {
        #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
        let mut total = 0u64;
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        let mut total = self.owned_bytes;
        for (index, region) in self.regions.iter().enumerate() {
            let Some(region) = region else { continue };
            let end = region.source_offset + region.length;
            let mut cursor = region.source_offset;
            while cursor < end {
                let mut next = end;
                let mut covered_until = cursor;
                for prior in self.regions[..index].iter().flatten() {
                    if !region.backing.same_storage(&prior.backing) {
                        continue;
                    }
                    let prior_end = prior.source_offset + prior.length;
                    if prior.source_offset <= cursor && cursor < prior_end {
                        covered_until = covered_until.max(prior_end.min(end));
                    } else if prior.source_offset > cursor {
                        next = next.min(prior.source_offset);
                    }
                }
                if covered_until > cursor {
                    cursor = covered_until;
                } else {
                    total = total
                        .checked_add(region.backing.resident_bytes(cursor, next - cursor)?)
                        .ok_or(MemoryObjectError::AllocationSize)?;
                    cursor = next;
                }
            }
        }
        Ok(total)
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl super::GuestAddressSpace {
    pub(crate) fn verify_resident_memory_for_test(
        domain: &ResourceDomain,
    ) -> Result<(), &'static str> {
        use crate::mm::user_space::VmoObject;

        let vmo = VmoObject::try_new_writable(8 * PAGE_SIZE, domain).map_err(|_| "VMO")?;
        let backing = GuestMemoryBacking::try_from_vmo(&vmo).map_err(|_| "RAM lease")?;
        let mut layout = Layout::try_new(16 * PAGE_SIZE, domain).map_err(|_| "layout")?;
        // Deliberately unordered overlapping source ranges, at disjoint GPAs.
        for (offset, source, pages) in [(0, 2, 2), (2, 0, 3), (5, 1, 7), (12, 2, 2)] {
            let mut region = Some(
                Region::new(
                    offset * PAGE_SIZE,
                    source * PAGE_SIZE,
                    pages * PAGE_SIZE,
                    backing.clone(),
                )
                .map_err(|_| "region")?,
            );
            layout.insert(&mut region).map_err(|_| "insert")?;
        }
        if layout
            .resident_memory()
            .bytes()
            .map_err(|_| "empty count")?
            != 0
        {
            return Err("inspection populated sparse RAM");
        }
        for page in [0, 2, 4, 6] {
            backing
                .populate_page(page * PAGE_SIZE)
                .map_err(|_| "populate")?;
        }
        // These physical pages have never been installed into any stage-2 table.
        if layout
            .resident_memory()
            .bytes()
            .map_err(|_| "sparse count")?
            != 4 * PAGE_SIZE
        {
            return Err("resident source aliases counted repeatedly");
        }
        let other_vmo = VmoObject::try_new_writable(PAGE_SIZE, domain).map_err(|_| "other VMO")?;
        let other = GuestMemoryBacking::try_from_vmo(&other_vmo).map_err(|_| "other lease")?;
        other.populate_page(0).map_err(|_| "other populate")?;
        let mut region =
            Some(Region::new(14 * PAGE_SIZE, 0, PAGE_SIZE, other).map_err(|_| "other region")?);
        layout.insert(&mut region).map_err(|_| "other insert")?;
        let retained = layout.resident_memory();
        drop(layout);
        drop(backing);
        drop(vmo);
        drop(other_vmo);
        if retained.bytes().map_err(|_| "retained count")? != 5 * PAGE_SIZE {
            return Err("snapshot lost backing or merged different VMOs");
        }
        Ok(())
    }
}

/// Coverage is checked before sealing; an unmapped gap never becomes anonymous RAM.
pub(crate) struct Layout {
    size: u64,
    regions: [Option<Region>; 8],
    _charge: CommittedCharge,
}

impl Layout {
    pub(crate) fn try_new(
        size: u64,
        domain: &ResourceDomain,
    ) -> Result<alloc::boxed::Box<Self>, super::Error> {
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                core::mem::size_of::<Self>() as u64,
            ))?
            .commit();
        hyper::mm::try_box(Self {
            size,
            regions: [const { None }; 8],
            _charge: charge,
        })
        .map_err(|_| super::Error::MetadataAllocation)
    }

    /// A rejected region stays caller-owned so its lease is dropped outside locks.
    pub(crate) fn insert(&mut self, region: &mut Option<Region>) -> Result<(), MemoryObjectError> {
        let candidate = region.as_ref().ok_or(MemoryObjectError::AllocationSize)?;
        let end = candidate.offset + candidate.length;
        if end > self.size
            || self
                .regions
                .iter()
                .flatten()
                .any(|old| candidate.offset < old.offset + old.length && old.offset < end)
        {
            return Err(MemoryObjectError::AllocationSize);
        }
        let slot = self
            .regions
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(MemoryObjectError::AllocationSize)?;
        *slot = region.take();
        Ok(())
    }

    pub(crate) fn resident_memory(&self) -> ResidentMemory {
        ResidentMemory {
            regions: self.regions.clone(),
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            owned_bytes: 0,
        }
    }

    pub(crate) fn complete(&self) -> bool {
        // Sealing requires actual backing, not coverage of the address-space
        // envelope. Unadmitted gaps remain unmapped and cannot fault in pages.
        self.regions.iter().any(Option::is_some)
    }

    pub(crate) fn overlaps(&self, offset: u64, length: u64) -> bool {
        self.extents()
            .any(|(base, size)| offset < base + size && base < offset + length)
    }

    pub(crate) fn contains(&self, offset: u64) -> bool {
        self.resolve(offset, 1).is_ok()
    }

    fn extents(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.regions
            .iter()
            .flatten()
            .map(|region| (region.offset, region.length))
    }

    pub(crate) fn page_count(&self) -> Result<usize, MemoryObjectError> {
        self.extents().try_fold(0usize, |total, (_, length)| {
            total
                .checked_add(
                    usize::try_from(length / PAGE_SIZE)
                        .map_err(|_| MemoryObjectError::AllocationSize)?,
                )
                .ok_or(MemoryObjectError::AllocationSize)
        })
    }

    pub(crate) fn page_index(&self, offset: u64) -> Option<usize> {
        super::extent_index::index(self.extents(), offset)
    }

    pub(crate) fn page_offset(&self, index: usize) -> Option<u64> {
        super::extent_index::offset(self.extents(), index)
    }

    pub(crate) fn table_capacity(&self, ipa_base: u64) -> Result<usize, super::Error> {
        self.extents().try_fold(0usize, |total, (offset, length)| {
            let base = ipa_base
                .checked_add(offset)
                .ok_or(super::Error::AddressOverflow)?;
            total
                .checked_add(crate::hal::vm::Stage2AddressSpace::required_table_pages(
                    base, length,
                )?)
                .ok_or(super::Error::MetadataAllocation)
        })
    }

    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    fn resolve(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<(&GuestMemoryBacking, u64), MemoryObjectError> {
        let end = offset
            .checked_add(length)
            .ok_or(MemoryObjectError::AllocationSize)?;
        self.regions
            .iter()
            .flatten()
            .find(|region| offset >= region.offset && end <= region.offset + region.length)
            .map(|region| {
                (
                    &region.backing,
                    region.source_offset + offset - region.offset,
                )
            })
            .ok_or(MemoryObjectError::AllocationSize)
    }

    /// DMA-owning VMs must retain resident backing for every advertised range.
    pub(crate) fn validate_dma(&self) -> Result<(), MemoryObjectError> {
        for region in self.regions.iter().flatten() {
            let first = region.backing.physical_page(region.source_offset)?.get();
            first
                .checked_add(region.length)
                .ok_or(MemoryObjectError::AllocationSize)?;
            for offset in (0..region.length).step_by(PAGE_SIZE as usize) {
                if region
                    .backing
                    .physical_page(region.source_offset + offset)?
                    .get()
                    != first + offset
                {
                    return Err(MemoryObjectError::AllocationSize);
                }
            }
        }
        Ok(())
    }

    /// Require one retained region to authorize the complete block. Adjacent
    /// regions need not share ownership even when their physical pages touch.
    pub(crate) fn covers_range(&self, offset: u64, length: u64) -> bool {
        self.resolve(offset, length).is_ok()
    }

    pub(crate) fn populate_page(&self, offset: u64) -> Result<(), MemoryObjectError> {
        let (backing, offset) = self.resolve(offset, PAGE_SIZE)?;
        backing.populate_page(offset)
    }
    pub(crate) fn physical_page(&self, offset: u64) -> Result<PhysicalAddress, MemoryObjectError> {
        let (backing, offset) = self.resolve(offset, PAGE_SIZE)?;
        backing.physical_page(offset)
    }
    pub(crate) fn page_is_resident(&self, offset: u64) -> Result<bool, MemoryObjectError> {
        let (backing, offset) = self.resolve(offset, PAGE_SIZE)?;
        backing.page_is_resident(offset)
    }
    pub(crate) fn read_exposed(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), MemoryObjectError> {
        let (backing, offset) = self.resolve(offset, destination.len() as u64)?;
        backing.read_exposed(offset, destination)
    }
    pub(crate) fn write_exposed(
        &self,
        offset: u64,
        source: &[u8],
    ) -> Result<(), MemoryObjectError> {
        let (backing, offset) = self.resolve(offset, source.len() as u64)?;
        backing.write_exposed(offset, source)
    }
}
