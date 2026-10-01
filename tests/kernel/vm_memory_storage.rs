// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Live table capacity failures escape the IRQ lock without committing owners.

use hyper::sync::InterruptSpinLock;

use super::{Error, Stage2PagePool, Target};
use crate::kernel::accounting::{ResourceDomain, ResourceError, ResourceKind, ResourceLimits};
use crate::kernel::vm::memory::GuestAddressSpace;

impl GuestAddressSpace {
    pub(crate) fn verify_live_capacity_for_test() -> Result<(), &'static str> {
        let parent = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)
            .map_err(|_| "live pool parent domain")?;
        let domain = parent
            .try_new_child(ResourceLimits::UNLIMITED)
            .map_err(|_| "live pool child domain")?;
        // The child domain itself is charged to its parent. Deny additional
        // pool storage only after both domain owners have been constructed.
        let parent_baseline = parent.usage().committed(ResourceKind::KernelMemoryBytes);
        let domain_baseline = domain.usage().committed(ResourceKind::KernelMemoryBytes);
        parent
            .set_local_limits(
                ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, parent_baseline),
            )
            .map_err(|_| "live pool denial quota")?;
        let pool =
            Stage2PagePool::with_capacity(0, &domain).map_err(|_| "empty live pool preparation")?;
        // Match the real address-space lock. reserve_live reports a recovery
        // target; it must not attempt the scheduled wait while this is held.
        let pool = InterruptSpinLock::<_, crate::hal::irq::LocalMask>::new(pool);
        let failure = pool
            .with(|pool| pool.reserve_live(1))
            .err()
            .ok_or("live pool accepted denied metadata")?;
        if failure.reclaim != Some(Target::Domain(parent.id()))
            || !matches!(failure.error, Error::Resource(ResourceError::LimitExceeded {
                domain: denied,
                resource: ResourceKind::KernelMemoryBytes,
                ..
            }) if denied == parent.id())
        {
            return Err("live pool lost denying ancestor identity");
        }
        if !pool.with(empty)
            || domain.usage().committed(ResourceKind::KernelMemoryBytes) != domain_baseline
            || parent.usage().committed(ResourceKind::KernelMemoryBytes) != parent_baseline
        {
            return Err("denied live pool retained preparation ownership");
        }

        // An impossible layout is a validation failure, not a reason to drain
        // the cache. It must leave the same unpublished pool untouched.
        let failure = pool
            .with(|pool| pool.reserve_live(usize::MAX))
            .err()
            .ok_or("live pool accepted an overflowing layout")?;
        if failure.error != Error::MetadataAllocation
            || failure.reclaim.is_some()
            || !pool.with(empty)
        {
            return Err("invalid live pool layout requested reclaim or changed ownership");
        }

        parent
            .set_local_limits(ResourceLimits::UNLIMITED)
            .map_err(|_| "live pool retry quota")?;
        pool.with(|pool| pool.reserve_live(4))
            .map_err(|_| "live pool retry after quota release")?;
        let expected = 4 * core::mem::size_of::<super::PageBlock>() as u64;
        if !pool.with(|pool| {
            pool.pages.is_empty()
                && pool.pages.capacity() == 4
                && pool.live_capacity_ready(4) == Ok(true)
        }) || domain.usage().committed(ResourceKind::KernelMemoryBytes)
            != domain_baseline + expected
            || parent.usage().committed(ResourceKind::KernelMemoryBytes)
                != parent_baseline + expected
        {
            return Err("live pool retry failed to retain exact metadata charge");
        }
        // Idempotent preparation does not charge the same capacity again.
        pool.with(|pool| pool.reserve_live(4))
            .map_err(|_| "live pool repeat preparation")?;
        if domain.usage().committed(ResourceKind::KernelMemoryBytes) != domain_baseline + expected {
            return Err("live pool repeat preparation charged twice");
        }
        drop(pool);
        if domain.usage().committed(ResourceKind::KernelMemoryBytes) != domain_baseline
            || parent.usage().committed(ResourceKind::KernelMemoryBytes) != parent_baseline
        {
            return Err("live pool retirement retained metadata charge");
        }
        Ok(())
    }
}

fn empty(pool: &mut Stage2PagePool) -> bool {
    pool.pages.is_empty()
        && pool.pages.capacity() == 0
        && pool.charge.is_none()
        && pool.error.is_none()
}
