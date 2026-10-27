// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit authority to share frozen writable backing among guest address spaces.

use super::{Error, reserve_object_charge};
use crate::authority::Rights;
use core::sync::atomic::{AtomicUsize, Ordering};
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {crate::mm::user_space::GuestMemoryBacking, crate::mm::user_space::VmoObject};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::TransferClass,
    crate::object::private,
};

const IDLE: usize = 0;
const KERNEL_INITIATOR: usize = 1;
const NATIVE_ACCESS: usize = 2;

pub(crate) struct GuestMemoryObject {
    backing: GuestMemoryBacking,
    native_access: AtomicUsize,
    _charge: CommittedCharge,
}

impl GuestMemoryObject {
    pub(crate) fn try_new(vmo: &VmoObject, domain: &ResourceDomain) -> Result<Self, Error> {
        let charge = reserve_object_charge::<Self>(domain)?;
        Ok(Self {
            backing: GuestMemoryBacking::try_from_vmo(vmo)?,
            native_access: AtomicUsize::new(IDLE),
            _charge: charge,
        })
    }
    pub(crate) fn claim_native_initiator(&self) -> bool {
        self.native_access
            .compare_exchange(IDLE, KERNEL_INITIATOR, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn native_access(&self) -> Result<NativeAccess<'_>, crate::vm::service::Error> {
        // Even readers exclude other Native operations: mixed-width atomic
        // words and byte copies must never overlap inside the Rust kernel.
        self.native_access
            .compare_exchange(IDLE, NATIVE_ACCESS, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| crate::vm::service::Error::Busy)?;
        Ok(NativeAccess(&self.native_access))
    }
    pub(crate) fn backing(&self) -> GuestMemoryBacking {
        self.backing.clone()
    }
}
impl private::Sealed for GuestMemoryObject {}
impl private::UserExportable for GuestMemoryObject {}
impl KernelObject for GuestMemoryObject {
    const KIND: ObjectKind = ObjectKind::GUEST_MEMORY;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::DUPLICATE)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::MAP)
        .union(Rights::INSPECT);
}

/// Serializes Native payload accesses and excludes kernel-initiator dedication.
pub(crate) struct NativeAccess<'a>(&'a AtomicUsize);
impl Drop for NativeAccess<'_> {
    fn drop(&mut self) {
        self.0.store(IDLE, Ordering::Release);
    }
}
