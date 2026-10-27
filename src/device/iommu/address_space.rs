// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A VM's DMA authority: only retained RAM leases enter its hardware domain.
//! All command submission runs in sleeping context, outside VM/device locks.

use super::runtime;
use alloc::vec::Vec;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind, crate::device::dma::Buffer,
    crate::device::dma::Lease, crate::sync::Mutex, crate::sync::MutexGuard,
};
use {
    hyper::mm::FallibleArc, hyper::mm::PAGE_SIZE, hyper_drivers::iommu::smmuv3::DomainId,
    hyper_drivers::iommu::smmuv3::Error, hyper_drivers::iommu::smmuv3::Permissions,
};

const MAX_GRANTS: usize = 9; // Initial RAM plus eight live backend mappings.

struct Grant {
    token: u64,
    bases: Vec<u64>,
    _charge: CommittedCharge,
}
struct State {
    closed: bool,
    retired: bool,
    attached: [Option<u32>; 4],
    grants: Vec<Grant>,
}

pub(crate) struct AddressSpace {
    runtime: FallibleArc<runtime::Runtime>,
    id: DomainId,
    accounting: ResourceDomain,
    state: Mutex<State>,
    _charge: CommittedCharge,
}

impl AddressSpace {
    /// Devices are still reset and bus mastering is disabled. A failed command
    /// leaves the driver's durable owners pinned; dropping this facade never
    /// attempts hardware cleanup or treats an error as an invalidation proof.
    pub(crate) fn create(
        streams: [Option<u32>; 4],
        leases: &[Option<Lease>],
        accounting: ResourceDomain,
    ) -> Result<FallibleArc<Self>, Error> {
        let runtime = runtime::owner().ok_or(Error::Unsupported)?;
        let charge = accounting
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                (FallibleArc::<Self>::allocation_size()
                    + MAX_GRANTS * core::mem::size_of::<Grant>()) as u64,
            ))
            .map_err(|_| Error::Allocation)?
            .commit();
        let mut grants = Vec::new();
        grants
            .try_reserve_exact(MAX_GRANTS)
            .map_err(|_| Error::Allocation)?;
        let id = runtime.with_controller(|controller| controller.create_domain())?;
        let candidate = Self {
            runtime,
            id,
            accounting,
            state: Mutex::new(State {
                closed: false,
                retired: false,
                attached: [None; 4],
                grants,
            }),
            _charge: charge,
        };
        let owner = match FallibleArc::try_new_or_return(candidate) {
            Ok(owner) => owner,
            Err((_, candidate)) => {
                let _ = candidate
                    .runtime
                    .with_controller(|controller| controller.destroy_domain(id));
                return Err(Error::Allocation);
            }
        };
        let result = (|| {
            owner.grant_leases(0, leases.iter().flatten())?;
            let mut state = owner.lock();
            for (index, stream) in streams.into_iter().enumerate() {
                let Some(stream) = stream else {
                    continue;
                };
                owner
                    .runtime
                    .with_controller(|controller| controller.attach(stream, id))?;
                state.attached[index] = Some(stream);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = owner.retire();
            return Err(error);
        }
        Ok(owner)
    }

    pub(crate) fn grant(&self, token: u64, lease: &Lease) -> Result<(), Error> {
        if token == 0 {
            return Err(Error::Address);
        }
        self.grant_leases(token, core::iter::once(lease))
    }

    #[cfg(all(
        CONFIG_ARCH_AARCH64,
        all(feature = "kernel-test", feature = "io-dma-revoke-test")
    ))]
    pub(super) fn revoke_test_page(&self, iova: u64) -> Result<Buffer, Error> {
        let state = self.lock();
        // The fixture uses the ordinary physical-net function in PCI slot 2.
        // Reject a stale or unrelated VM; never create a second test domain.
        if state.closed || !state.attached.contains(&Some(16)) {
            return Err(Error::InvalidDomain);
        }
        self.runtime
            .with_controller(|controller| controller.revoke_test_page(self.id, iova))
    }

    fn grant_leases<'a>(
        &self,
        token: u64,
        leases: impl Iterator<Item = &'a Lease> + Clone,
    ) -> Result<(), Error> {
        let count = leases.clone().try_fold(0usize, |count, lease| {
            count
                .checked_add(runs(lease).count())
                .ok_or(Error::Allocation)
        })?;
        let bytes = count
            .checked_mul(core::mem::size_of::<u64>())
            .ok_or(Error::Allocation)?;
        let charge = self
            .accounting
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
            .map_err(|_| Error::Allocation)?
            .commit();
        let mut bases = Vec::new();
        bases
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        let mut state = self.lock();
        if state.closed
            || state.grants.len() == MAX_GRANTS
            || state.grants.iter().any(|g| g.token == token)
        {
            return Err(Error::DomainBusy);
        }
        // Allocate every tracking owner before publication. If command failure
        // prevents rollback, the controller itself retains every published lease.
        for lease in leases {
            for (offset, length) in runs(lease) {
                let base = lease.object().physical_page(offset);
                let buffer = Buffer::slice(lease.clone(), offset, length)?;
                if let Err(error) = self.runtime.with_controller(|controller| {
                    controller.map_buffer(self.id, base, buffer, Permissions::ReadWrite)
                }) {
                    for base in bases {
                        let _ = self
                            .runtime
                            .with_controller(|controller| controller.unmap_buffer(self.id, base));
                    }
                    return Err(error);
                }
                bases.push(base);
            }
        }
        state.grants.push(Grant {
            token,
            bases,
            _charge: charge,
        });
        Ok(())
    }

    /// The caller has withdrawn CPU admission. No retained RAM is released until
    /// the hardware invalidation completes, regardless of the backend's reply.
    pub(crate) fn revoke(&self, token: u64) -> Result<(), Error> {
        let mut state = self.lock();
        let Some(index) = state.grants.iter().position(|grant| grant.token == token) else {
            return Ok(());
        };
        self.revoke_grant(&mut state.grants[index])?;
        state.grants.swap_remove(index);
        Ok(())
    }

    fn revoke_grant(&self, grant: &mut Grant) -> Result<(), Error> {
        while let Some(base) = grant.bases.last().copied() {
            let buffer = self
                .runtime
                .with_controller(|controller| controller.unmap_buffer(self.id, base))?;
            grant.bases.pop();
            drop(buffer);
        }
        Ok(())
    }

    /// Call after physical reset acknowledgement. Closing admission serializes
    /// racing live grants; stream denial precedes page release and domain reuse.
    pub(crate) fn retire(&self) -> Result<(), Error> {
        let mut state = self.lock();
        if state.retired {
            return Ok(());
        }
        state.closed = true;
        for slot in &mut state.attached {
            if let Some(stream) = *slot {
                self.runtime
                    .with_controller(|controller| controller.detach(stream))?;
                *slot = None;
            }
        }
        while let Some(grant) = state.grants.last_mut() {
            self.revoke_grant(grant)?;
            state.grants.pop();
        }
        self.runtime
            .with_controller(|controller| controller.destroy_domain(self.id))?;
        state.retired = true;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| {
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: DMA address-space lock failed: {error:?}"
            ))
        })
    }
}

fn runs(lease: &Lease) -> impl Iterator<Item = (u64, u64)> + '_ {
    let mut offset = 0;
    core::iter::from_fn(move || {
        if offset == lease.object().length() {
            return None;
        }
        let start = offset;
        let base = lease.object().physical_page(start);
        offset += PAGE_SIZE;
        while offset < lease.object().length()
            && lease.object().physical_page(offset) == base + offset - start
        {
            offset += PAGE_SIZE;
        }
        Some((start, offset - start))
    })
}
