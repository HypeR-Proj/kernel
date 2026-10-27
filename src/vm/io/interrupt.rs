// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exclusively reserved guest level interrupts for userspace device models.

use super::{Error, Route};
use crate::authority::Rights;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::TransferClass, crate::object::private,
};
use {crate::vm::installed::InstalledMachine, crate::vm::registry::VmId};

pub(crate) struct Shared {
    vm: VmId,
    pub(super) irq: u32,
    // The source lock serializes level changes, closure and saved GIC updates.
    state: InterruptSpinLock<bool, crate::hal::irq::LocalMask>,
    installed: AtomicBool,
    _charge: CommittedCharge,
}
impl Shared {
    fn set_level(&self, asserted: bool) -> Result<(), Error> {
        super::with_irq_binding(self.vm, |binding| {
            self.state.with(|closed| {
                if *closed {
                    return Err(Error::BadState);
                }
                super::set_line(binding, self.irq, asserted);
                Ok(())
            })?;
            binding.publish_changed_interrupts();
            Ok(())
        })?
    }
    pub(super) fn close(&self) {
        if !self.installed.load(Ordering::Acquire) {
            self.state.with(|closed| *closed = true);
            return;
        }
        if super::with_irq_binding(self.vm, |binding| {
            self.state.with(|closed| {
                *closed = true;
                super::set_line(binding, self.irq, false);
            });
            binding.publish_changed_interrupts();
        })
        .is_err()
        {
            self.state.with(|closed| *closed = true);
        }
    }
}

#[derive(Clone)]
pub(crate) struct Interrupt {
    // This object retains the lifecycle owner; the VM route retains only Shared,
    // so there is no VM -> object -> VM ownership cycle.
    machine: FallibleArc<InstalledMachine>,
    shared: FallibleArc<Shared>,
}
impl Interrupt {
    pub(crate) fn prepare(
        machine: FallibleArc<InstalledMachine>,
        irq: u32,
        domain: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !super::valid_irq(irq) {
            return Err(Error::InvalidArgument);
        }
        let vm = machine.io_update_id()?;
        let shared = FallibleArc::try_new(Shared {
            vm,
            irq,
            state: InterruptSpinLock::new(false),
            installed: AtomicBool::new(false),
            _charge: super::charge::<Self, Shared>(domain)?,
        })
        .map_err(|_| Error::NoMemory)?;
        Ok(Self { machine, shared })
    }
    pub(crate) fn install(&self) -> Result<(), Error> {
        self.machine.with_io_update(|id| {
            if id != self.shared.vm {
                return Err(Error::BadState);
            }
            let route = Route::Interrupt(self.shared.clone());
            super::with_irq_binding(id, |binding| {
                binding.preflight_io_route(&route)?;
                binding.install_io_route(route)?;
                self.shared.installed.store(true, Ordering::Release);
                Ok(())
            })?
        })
    }
    pub(crate) fn set_level(&self, asserted: bool) -> Result<(), Error> {
        self.machine
            .with_io_update(|_| self.shared.set_level(asserted))
    }
}
impl private::Sealed for Interrupt {}
impl private::UserExportable for Interrupt {}
impl KernelObject for Interrupt {
    const KIND: ObjectKind = ObjectKind::GUEST_INTERRUPT;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::WRITE.union(Rights::TRANSFER).union(Rights::INSPECT);
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.shared.close();
    }
}
