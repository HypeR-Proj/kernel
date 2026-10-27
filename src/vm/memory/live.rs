// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Device mappings retain stable VMO backing independently of Native handles.
//! Only the admitted backend route can certify DMA quiescence.

use super::{Error, grant_state::State};
use crate::mm::user_space::GuestMemoryBacking;
use alloc::vec::Vec;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};

pub(crate) const ALIAS_OFFSET: u64 = hyper::abi::native::HYPER_NATIVE_GUEST_DYNAMIC_ALIAS_OFFSET;
pub(crate) const PHYSICAL_LIMIT: u64 =
    hyper::abi::native::HYPER_NATIVE_GUEST_DYNAMIC_PHYSICAL_LIMIT;
const PAGE: u64 = 4096;
pub(crate) const MAX_MAPPINGS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Extent {
    pub(crate) alias: u64,
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

pub(crate) struct Mapping {
    pub(crate) token: u64,
    pub(in crate::vm) state: State,
    pub(crate) frontend: u64,
    pub(crate) length: u64,
    pub(crate) extents: Vec<Extent>,
    table_capacity: usize,
    // A physical-order index permits logarithmic fault lookup while the
    // immutable extent list remains in frontend order for vhost registration.
    order: Vec<usize>,
    pub(super) lease: crate::device::dma::Lease,
    _charge: CommittedCharge,
}

impl Mapping {
    pub(crate) fn prepare(
        backing: GuestMemoryBacking,
        frontend: u64,
        domain: &ResourceDomain,
    ) -> Result<Self, Error> {
        let length = backing.size();
        if length == 0 || !frontend.is_multiple_of(PAGE) || frontend.checked_add(length).is_none() {
            return Err(Error::InvalidRange);
        }
        let count = usize::try_from(length / PAGE).map_err(|_| Error::InvalidRange)?;
        let bytes = count
            .checked_mul(core::mem::size_of::<Extent>() + core::mem::size_of::<usize>())
            .and_then(|n| n.checked_add(core::mem::size_of::<Self>()))
            .ok_or(Error::MetadataAllocation)?;
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))?
            .commit();
        let mut extents: Vec<Extent> = Vec::new();
        crate::mm::reclaim::reserve_exact(&mut extents, count)
            .map_err(|_| Error::MetadataAllocation)?;
        let lease = crate::device::dma::BackendMemoryLease::prepare(backing, domain)?;
        for offset in (0..length).step_by(PAGE as usize) {
            let physical = lease.object().physical_page(offset);
            if physical
                .checked_add(PAGE)
                .is_none_or(|end| end > PHYSICAL_LIMIT)
            {
                return Err(Error::InvalidRange);
            }
            let alias = ALIAS_OFFSET + physical;
            if let Some(last) = extents.last_mut()
                && last.alias + last.length == alias
            {
                last.length += PAGE;
            } else {
                extents.push(Extent {
                    alias,
                    offset,
                    length: PAGE,
                });
            }
        }
        let mut order = Vec::new();
        crate::mm::reclaim::reserve_exact(&mut order, count)
            .map_err(|_| Error::MetadataAllocation)?;
        order.extend(0..extents.len());
        order.sort_unstable_by_key(|index| extents[*index].alias);
        // A grant is an ordinary writable VMO: duplicate physical pages would
        // make vhost's frontend view ambiguous and are rejected explicitly.
        for pair in order.windows(2) {
            let a = extents[pair[0]];
            let b = extents[pair[1]];
            if a.alias + a.length > b.alias {
                return Err(Error::InvalidRange);
            }
        }
        let table_capacity = extents.iter().try_fold(0usize, |total, extent| {
            total
                .checked_add(crate::hal::vm::Stage2AddressSpace::required_table_pages(
                    extent.alias,
                    extent.length,
                )?)
                .ok_or(Error::MetadataAllocation)
        })?;
        Ok(Self {
            token: 0,
            state: State::Created,
            frontend,
            length,
            extents,
            table_capacity,
            order,
            lease,
            _charge: charge,
        })
    }

    fn covers_range(&self, address: u64, length: u64) -> bool {
        let Some(end) = address.checked_add(length) else {
            return false;
        };
        let index = self
            .order
            .partition_point(|index| self.extents[*index].alias <= address);
        index.checked_sub(1).is_some_and(|index| {
            let extent = self.extents[self.order[index]];
            end <= extent.alias + extent.length
        })
    }

    pub(crate) fn contains(&self, address: u64) -> bool {
        let index = self
            .order
            .partition_point(|index| self.extents[*index].alias <= address);
        index.checked_sub(1).is_some_and(|index| {
            let extent = self.extents[self.order[index]];
            address < extent.alias + extent.length
        })
    }
}

pub(crate) struct LiveMappings {
    pub(crate) slots: [Option<alloc::boxed::Box<Mapping>>; MAX_MAPPINGS],
    next_token: u64,
}

impl LiveMappings {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { None }; MAX_MAPPINGS],
            next_token: 1,
        }
    }

    fn can_insert(&self, candidate: &Mapping) -> Result<(), Error> {
        if candidate.token == 0 {
            return Err(Error::InvalidRange);
        }
        if self.slots.iter().all(Option::is_some) {
            return Err(Error::MetadataAllocation);
        }
        // Affine aliases are unique to physical memory. Overlapping concurrent
        // grants would allow one route to unmap another's DMA, so reject them.
        for old in self.slots.iter().flatten() {
            for extent in &candidate.extents {
                let position = old.order.partition_point(|index| {
                    old.extents[*index].alias < extent.alias + extent.length
                });
                if position.checked_sub(1).is_some_and(|index| {
                    let prior = old.extents[old.order[index]];
                    prior.alias + prior.length > extent.alias
                }) {
                    return Err(Error::InvalidRange);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn insert(
        &mut self,
        mapping: &mut Option<alloc::boxed::Box<Mapping>>,
    ) -> Result<u64, Error> {
        let candidate = mapping.as_ref().ok_or(Error::InvalidRange)?;
        self.can_insert(candidate)?;
        let token = candidate.token;
        if token == 0 {
            return Err(Error::InvalidRange);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::MetadataAllocation)?;
        *slot = mapping.take();
        Ok(token)
    }

    pub(crate) fn reserve_token(&mut self) -> Result<u64, Error> {
        let next = self.next_token.checked_add(1).ok_or(Error::InvalidRange)?;
        let token = self.next_token;
        self.next_token = next;
        Ok(token)
    }

    pub(crate) fn find_mut(&mut self, token: u64) -> Option<&mut Mapping> {
        self.slots
            .iter_mut()
            .flatten()
            .find(|mapping| mapping.token == token)
            .map(|mapping| mapping.as_mut())
    }

    pub(super) fn table_capacity(&self) -> Result<usize, Error> {
        self.slots
            .iter()
            .flatten()
            .try_fold(0usize, |total, mapping| {
                total
                    .checked_add(mapping.table_capacity)
                    .ok_or(Error::MetadataAllocation)
            })
    }
}

impl super::GuestAddressSpace {
    /// Allocate pool metadata before entering the VM lifecycle commit gate.
    pub(crate) fn prepare_live_install(
        &mut self,
        candidate: &Mapping,
    ) -> Result<(), super::storage::LivePreparationError> {
        let capacity = self.validate_live_install(candidate)?;
        self.table_pages.reserve_live(capacity)
    }

    /// None means a concurrent fault/admission consumed the prepared metadata
    /// capacity. No state changed; reserve again outside the lifecycle lock.
    pub(crate) fn install_live(
        &mut self,
        mapping: &mut Option<alloc::boxed::Box<Mapping>>,
    ) -> Result<Option<u64>, Error> {
        let capacity = self.validate_live_install(mapping.as_ref().ok_or(Error::InvalidRange)?)?;
        if !self.table_pages.live_capacity_ready(capacity)? {
            return Ok(None);
        }
        self.live.insert(mapping).map(Some)
    }

    fn validate_live_install(&self, candidate: &Mapping) -> Result<usize, Error> {
        self.ensure_healthy()?;
        let end = self
            .ipa_base
            .checked_add(self.size)
            .ok_or(Error::InvalidRange)?;
        let capacity = self
            .live
            .table_capacity()?
            .checked_add(candidate.table_capacity)
            .ok_or(Error::MetadataAllocation)?;
        for extent in &candidate.extents {
            if extent.alias < self.ipa_base || extent.alias + extent.length > end {
                return Err(Error::InvalidRange);
            }
            // Existing initial backing must never overlap the dynamic window.
            let overlaps = match &self.backing {
                super::storage::GuestMemoryBacking::SharedVmo(layout) => {
                    layout.overlaps(extent.alias - self.ipa_base, extent.length)
                }
                #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
                super::storage::GuestMemoryBacking::KernelOwned(_) => true,
            };
            if overlaps {
                return Err(Error::InvalidRange);
            }
        }
        self.live.can_insert(candidate)?;
        Ok(capacity)
    }

    pub(super) fn resolve_live_fault(
        &mut self,
        fault: hyper_vm_models::exit::GuestMemoryFault,
    ) -> Result<bool, Error> {
        self.ensure_table_writable()?;
        if fault.access() == hyper_vm_models::exit::MemoryAccess::Execute {
            return Ok(false);
        }
        let ipa = fault.address().get() & !(PAGE - 1);
        if !self
            .live
            .slots
            .iter()
            .flatten()
            .any(|mapping| matches!(mapping.state, State::Admitted { .. }) && mapping.contains(ipa))
        {
            return Ok(false);
        }
        let cpu = crate::cpu::current_index().ok_or(Error::InvalidCpu)?;
        self.residency
            .check_active(cpu.get(), self.translation_epoch)
            .map_err(Error::Residency)?;
        // Each extent retains one contiguous run of the same grant. Never
        // combine neighboring grants: their DMA retirement is independent.
        if let Some(size) = crate::hal::vm::Stage2AddressSpace::normal_block_size() {
            let base = ipa & !(size - 1);
            let eligible = self.live.slots.iter().flatten().any(|mapping| {
                matches!(mapping.state, State::Admitted { .. }) && mapping.covers_range(base, size)
            });
            if eligible {
                let mut allocate =
                    |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
                // SAFETY: The active VM lock protects the admitted record,
                // whose lease keeps the whole extent stable through retirement.
                // Affine aliases preserve physical alignment and exact coverage.
                let result = unsafe {
                    self.stage2.try_map_normal_block_active(
                        base,
                        base - ALIAS_OFFSET,
                        hyper_vm_models::translation::Stage2PagePermissions::ReadWrite,
                        &mut allocate,
                    )
                };
                match result {
                    Ok(true) => return Ok(true),
                    Ok(false) => {}
                    Err(hyper_vm_models::translation::ActiveMappingError::BeforeInstall(
                        crate::hal::vm::Stage2Error::Allocation,
                    )) => {
                        // Optional block admission must not prevent 4 KiB
                        // progress under table-allocation pressure.
                        let _ = self.table_pages.take_error();
                    }
                    Err(hyper_vm_models::translation::ActiveMappingError::BeforeInstall(error)) => {
                        return Err(error.into());
                    }
                    Err(
                        hyper_vm_models::translation::ActiveMappingError::InstalledButInvalidationFailed(
                            error,
                        ),
                    ) => {
                        self.poisoned = true;
                        return Err(Error::Stage2(error));
                    }
                }
            }
        }
        let result = {
            let mut allocate =
                |pages, alignment| self.table_pages.allocate_zeroed(pages, alignment);
            // SAFETY: this fault is dispatched with this VM active; its lock
            // serializes publication and the live record retains stable pages.
            unsafe {
                self.stage2.map_normal_page_active(
                    ipa,
                    ipa - ALIAS_OFFSET,
                    hyper_vm_models::translation::Stage2PagePermissions::ReadWrite,
                    &mut allocate,
                )
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(hyper_vm_models::translation::ActiveMappingError::BeforeInstall(error)) => {
                Err(self
                    .table_pages
                    .take_error()
                    .unwrap_or(Error::Stage2(error)))
            }
            Err(
                hyper_vm_models::translation::ActiveMappingError::InstalledButInvalidationFailed(
                    error,
                ),
            ) => {
                self.poisoned = true;
                Err(Error::Stage2(error))
            }
        }
    }
}
