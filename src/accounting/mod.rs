// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Hierarchical ownership and quota policy for kernel-managed resources.

#[cfg(not(test))]
mod object;
mod resource_domain;

#[cfg(not(test))]
pub(crate) use object::{ResourceDomainObject, ResourceDomainObjectError};

pub(crate) use resource_domain::{
    ChargeReservation, CommittedCharge, ResourceAmount, ResourceDomain, ResourceDomainId,
    ResourceError, ResourceKind, ResourceLimits,
};
