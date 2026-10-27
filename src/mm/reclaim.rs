// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduled reclaim before retrying unpublished allocation preparation.

use alloc::collections::TryReserveError;
use alloc::vec::Vec;
use core::alloc::Layout;

use crate::accounting::ResourceDomainId;
use crate::fs::vfs::CachePage;
use {crate::fs::cache::AdmissionPause, crate::fs::cache::worker};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Target {
    PhysicalOrder(usize),
    Domain(ResourceDomainId),
}

pub(crate) type ReclaimGuard = AdmissionPause<'static, CachePage>;

/// Runs one finite worker pass and retains a pause on new cache admissions.
///
/// This is an explicit thread-context operation, never an allocator callback.
/// Callers must not hold IRQ, spin, scheduler or cache locks. Sleeping locks
/// are permitted only when the worker never waits for them; domain cleanup is
/// consequently best effort and never waits for a filesystem mutex.
pub(crate) fn reclaim(target: Target) -> Option<ReclaimGuard> {
    worker::reclaim(target)
}

/// Selects reclaimable denials from Native mapping preparation. The quota's
/// denying domain may be an ancestor of the process that faults. Untyped heap
/// allocation failures deliberately remain failures: they do not identify the
/// contiguous order required by their scratch buffer or mapping metadata.
pub(crate) fn user_memory_target(error: &super::user_space::MachineError) -> Option<Target> {
    use super::user_space::{AddressSpaceError, KernelPageError, MachineError, VmoError};
    use {crate::accounting::ResourceError, crate::accounting::ResourceKind};

    let resource = match error {
        MachineError::Resource(error)
        | MachineError::Logical(AddressSpaceError::Account(error))
        | MachineError::Vmo(VmoError::Account(error)) => error,
        MachineError::Page(crate::mm::allocator::BuddyError::OutOfMemory)
        | MachineError::Logical(AddressSpaceError::Backend(KernelPageError::Allocation(
            crate::mm::allocator::BuddyError::OutOfMemory,
        )))
        | MachineError::Vmo(VmoError::Backend(KernelPageError::Allocation(
            crate::mm::allocator::BuddyError::OutOfMemory,
        ))) => return Some(Target::PhysicalOrder(0)),
        _ => return None,
    };
    match resource {
        ResourceError::LimitExceeded {
            domain,
            resource: ResourceKind::CommittedPages | ResourceKind::KernelMemoryBytes,
            ..
        } => Some(Target::Domain(*domain)),
        _ => None,
    }
}

/// Retry only a preparation whose failed attempt leaves no published effects.
/// Every failed attempt must destroy its partial owners before returning. The
/// request serializer is released before `operation` runs again; the separate
/// admission pause remains held through that attempt. Competing ordinary
/// allocations can still win the released backing, so retries are bounded.
pub(crate) fn retry_prepare<T, E>(
    target: Target,
    operation: impl FnMut() -> Result<T, E>,
    is_allocation_error: impl Fn(&E) -> bool,
) -> Result<T, E> {
    retry_prepare_with(operation, |error| {
        is_allocation_error(error).then_some(target)
    })
}

/// Select the exact physical or quota denial from each preparation failure.
/// The same scheduling and rollback requirements as `retry_prepare` apply.
pub(crate) fn retry_prepare_with<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    target_for_error: impl Fn(&E) -> Option<Target>,
) -> Result<T, E> {
    let mut result = operation();
    for _ in 0..2 {
        let Err(error) = &result else {
            break;
        };
        let Some(target) = target_for_error(error) else {
            break;
        };
        let Some(guard) = reclaim(target) else {
            break;
        };
        result = operation();
        drop(guard);
    }
    result
}

/// Fallible vector growth at an audited scheduled preparation boundary.
/// This must not be used by cache loaders, cache metadata growth, or IRQ code.
pub(crate) fn reserve_exact<T>(
    values: &mut Vec<T>,
    additional: usize,
) -> Result<(), TryReserveError> {
    let target = values
        .len()
        .checked_add(additional)
        .and_then(|length| Layout::array::<T>(length).ok())
        .and_then(super::cache_memory::allocation_page_bound)
        .map(|pages| Target::PhysicalOrder(pages.trailing_zeros() as usize));
    match target {
        Some(target) => retry_prepare(target, || values.try_reserve_exact(additional), |_| true),
        None => values.try_reserve_exact(additional),
    }
}
