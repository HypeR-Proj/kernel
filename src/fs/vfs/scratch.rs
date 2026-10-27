// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Request-owned scratch admission, independent of capability creator lifetime.

use crate::fs::primitives::file_data::StorageBudget;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};

#[derive(Clone)]
pub(crate) struct ScratchBudget(ResourceDomain);
impl ScratchBudget {
    pub(crate) fn new(sponsor: &ResourceDomain) -> Self {
        Self(sponsor.clone())
    }
}
impl StorageBudget for ScratchBudget {
    type Charge = CommittedCharge;
    type Error = ResourceError;
    fn reserve(&self, bytes: usize) -> Result<CommittedCharge, ResourceError> {
        self.0
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
            .map(|charge| charge.commit())
    }
}
pub(crate) type ScratchVec<T> = crate::fs::primitives::scratch::BudgetedVec<T, ScratchBudget>;
pub(crate) type ScratchString = crate::fs::primitives::scratch::BudgetedString<ScratchBudget>;

/// Prepare byte scratch before backend access or user-copy side effects.
/// Capacity overflow and unrelated quota errors remain terminal; only the
/// exact physical backing or denying memory domain requests scheduled reclaim.
pub(crate) fn resize_bytes(
    values: &mut ScratchVec<u8>,
    length: usize,
) -> Result<(), crate::fs::primitives::scratch::Error<ResourceError>> {
    use crate::fs::primitives::scratch::Error;
    use {crate::mm::reclaim::Target, crate::mm::reclaim::retry_prepare_with};

    retry_prepare_with(
        || values.resize(length, 0),
        |error| match error {
            Error::Allocation => core::alloc::Layout::array::<u8>(length)
                .ok()
                .and_then(crate::mm::cache_memory::allocation_page_bound)
                .map(|pages| Target::PhysicalOrder(pages.trailing_zeros() as usize)),
            Error::Budget(ResourceError::LimitExceeded {
                domain,
                resource: ResourceKind::KernelMemoryBytes,
                ..
            }) => Some(Target::Domain(*domain)),
            _ => None,
        },
    )
}

impl From<crate::fs::primitives::scratch::Error<ResourceError>> for super::Error {
    fn from(error: crate::fs::primitives::scratch::Error<ResourceError>) -> Self {
        match error {
            crate::fs::primitives::scratch::Error::Allocation => Self::Allocation,
            crate::fs::primitives::scratch::Error::Size => Self::InvalidPath,
            crate::fs::primitives::scratch::Error::Budget(error) => Self::Resource(error),
        }
    }
}
impl From<crate::fs::primitives::scratch::Error<ResourceError>> for super::instance::Error {
    fn from(error: crate::fs::primitives::scratch::Error<ResourceError>) -> Self {
        match error {
            crate::fs::primitives::scratch::Error::Allocation => Self::Allocation,
            crate::fs::primitives::scratch::Error::Size => Self::InvalidInput,
            crate::fs::primitives::scratch::Error::Budget(error) => Self::Resource(error),
        }
    }
}
