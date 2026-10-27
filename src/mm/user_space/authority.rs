// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit authority for publishing immutable native executable memory.

use crate::authority::Rights;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

/// Failure while preparing an accounted executable-authority object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(all(feature = "kernel-test", feature = "kernel-self-test")),
    expect(
        dead_code,
        reason = "reserved authority is currently constructed only by capability self-tests"
    )
)]
pub(crate) enum ExecutableAuthorityError {
    AllocationSize,
    Resource(ResourceError),
}

impl From<ResourceError> for ExecutableAuthorityError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Capability required to derive executable provenance from writable bytes.
///
/// The payload is intentionally stateless. Possession of the object reference
/// proves only identity; the `CREATE_EXECUTABLE` right on the resolving handle
/// grants the operation. Writable VMO authority is a distinct required input.
#[cfg_attr(
    not(all(feature = "kernel-test", feature = "kernel-self-test")),
    expect(
        dead_code,
        reason = "reserved authority is currently constructed only by capability self-tests"
    )
)]
pub(crate) struct ExecutableAuthority {
    _object_charge: CommittedCharge,
}

impl ExecutableAuthority {
    #[cfg_attr(
        not(all(feature = "kernel-test", feature = "kernel-self-test")),
        expect(
            dead_code,
            reason = "reserved authority is currently constructed only by capability self-tests"
        )
    )]
    pub(crate) fn try_new(sponsor: &ResourceDomain) -> Result<Self, ExecutableAuthorityError> {
        let bytes = object_allocation_size::<Self>()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(ExecutableAuthorityError::AllocationSize)?;
        let charge = sponsor
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, bytes),
            )?
            .commit();
        Ok(Self {
            _object_charge: charge,
        })
    }

    /// Creates the private proof consumed by the memory mechanism.
    ///
    /// Callers may reach this method only after resolving an authority handle
    /// with `CREATE_EXECUTABLE`; the proof itself never crosses the subsystem.
    #[expect(
        dead_code,
        reason = "reserved conversion has no Native syscall consumer yet"
    )]
    pub(super) const fn provenance(&self) -> super::ExecutableProvenance {
        super::ExecutableProvenance::for_capability()
    }
}

impl private::Sealed for ExecutableAuthority {}
impl private::UserExportable for ExecutableAuthority {}

impl KernelObject for ExecutableAuthority {
    const KIND: ObjectKind = ObjectKind::EXECUTABLE_AUTHORITY;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::CREATE_EXECUTABLE);
}
