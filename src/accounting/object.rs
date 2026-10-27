// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Userspace capability over one hierarchical resource-accounting domain.

use super::{
    CommittedCharge, ResourceAmount, ResourceDomain, ResourceError, ResourceKind, ResourceLimits,
};
use crate::authority::Rights;
use {
    crate::object::KernelObject, crate::object::ObjectCreationError, crate::object::ObjectKind,
    crate::object::ObjectPublication, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

/// Failure while preparing an accounted `ResourceDomain` capability object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResourceDomainObjectError {
    AlreadyPublished,
    AllocationSize,
    Object(ObjectCreationError),
    Resource(ResourceError),
}

impl From<ObjectCreationError> for ResourceDomainObjectError {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<ResourceError> for ResourceDomainObjectError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Handle-visible authority over one accounting-domain owner.
///
/// The inner domain remains the single accounting state machine. This object
/// adds only KOID identity, active-handle accounting, and rights validation.
pub(crate) struct ResourceDomainObject {
    domain: ResourceDomain,
    _object_charge: CommittedCharge,
}

impl ResourceDomainObject {
    fn try_new(domain: ResourceDomain) -> Result<Self, ResourceDomainObjectError> {
        let bytes = object_allocation_size::<Self>()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(ResourceDomainObjectError::AllocationSize)?;
        let charge = domain
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, bytes),
            )?
            .commit();
        Ok(Self {
            domain,
            _object_charge: charge,
        })
    }

    pub(crate) const fn domain(&self) -> &ResourceDomain {
        &self.domain
    }

    /// Constructs the single userspace object identity for this domain.
    pub(crate) fn try_publication(
        domain: ResourceDomain,
    ) -> Result<ObjectPublication<Self>, ResourceDomainObjectError> {
        if !domain.claim_object_publication() {
            return Err(ResourceDomainObjectError::AlreadyPublished);
        }
        let result = Self::try_new(domain.clone())
            .and_then(|payload| ObjectPublication::try_new(payload).map_err(Into::into));
        if result.is_err() {
            domain.abort_object_publication();
        }
        result
    }

    pub(crate) fn try_new_child(
        &self,
        limits: ResourceLimits,
    ) -> Result<ObjectPublication<Self>, ResourceDomainObjectError> {
        Self::try_publication(self.domain.try_new_child(limits)?)
    }
}

impl private::Sealed for ResourceDomainObject {}
impl private::UserExportable for ResourceDomainObject {}

impl KernelObject for ResourceDomainObject {
    const KIND: ObjectKind = ObjectKind::RESOURCE_DOMAIN;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::CREATE_RESOURCE_DOMAIN)
        .union(Rights::SET_LIMITS)
        .union(Rights::REVOKE)
        .union(Rights::RESOURCE_DOMAIN_SPONSOR);
}
