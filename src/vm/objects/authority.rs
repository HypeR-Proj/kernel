// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VM creation authority and resource-domain-bound one-shot lease objects.

use super::{Error, reserve_object_charge};
use crate::authority::Rights;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::TransferClass,
    crate::object::private,
};

/// Root authority delegated only to the initial userspace supervisor.
pub(crate) struct VirtualMachineCreationAuthority {
    _object_charge: CommittedCharge,
}

impl VirtualMachineCreationAuthority {
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(dead_code)
    )]
    pub(crate) fn try_new(sponsor: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn derive(
        &self,
        domain: &ResourceDomain,
    ) -> Result<VirtualMachineCreationLease, Error> {
        VirtualMachineCreationLease::try_new(domain)
    }
}

impl private::Sealed for VirtualMachineCreationAuthority {}
impl private::UserExportable for VirtualMachineCreationAuthority {}

impl KernelObject for VirtualMachineCreationAuthority {
    const KIND: ObjectKind = ObjectKind::VIRTUAL_MACHINE_CREATION_AUTHORITY;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::DERIVE)
        .union(Rights::CREATE_VIRTUAL_MACHINE)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS);
}

/// One-shot authority to construct a VM charged to one resource domain.
pub(crate) struct VirtualMachineCreationLease {
    domain: ResourceDomain,
    _object_charge: CommittedCharge,
}

impl VirtualMachineCreationLease {
    fn try_new(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            domain: domain.clone(),
            _object_charge: reserve_object_charge::<Self>(domain)?,
        })
    }

    pub(crate) const fn domain(&self) -> &ResourceDomain {
        &self.domain
    }
}

impl private::Sealed for VirtualMachineCreationLease {}
impl private::UserExportable for VirtualMachineCreationLease {}

impl KernelObject for VirtualMachineCreationLease {
    const KIND: ObjectKind = ObjectKind::VIRTUAL_MACHINE_CREATION_LEASE;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::INSPECT)
        .union(Rights::CREATE_VIRTUAL_MACHINE)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS);
}
