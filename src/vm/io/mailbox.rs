// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Preallocated duplex control mailbox. Payloads never enter guest-owned RAM.

use super::super::{installed::InstalledMachine, registry::VmId};
use super::{Error, Route, model};
use crate::authority::Rights;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalState,
    crate::object::TransferClass, crate::object::private,
};
use {
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

pub(crate) struct Shared {
    pub(super) vm: VmId,
    pub(super) base: u64,
    pub(super) irq: u32,
    state: InterruptSpinLock<model::MailboxState, crate::hal::irq::LocalMask>,
    signals: SignalState,
    installed: AtomicBool,
    _charge: CommittedCharge,
}
impl Shared {
    fn native_signal(&self, state: &model::MailboxState) {
        if self
            .signals
            .update(
                SignalMask::from_trusted_bits(7),
                SignalMask::from_trusted_bits(state.native_status()),
            )
            .is_err()
        {
            hyper::debug::invariant_failure("vm::io::mailbox::native_signal invariant");
        }
    }
    fn mutate<R>(&self, operation: impl FnOnce(&mut model::MailboxState) -> R) -> Result<R, Error> {
        super::with_irq_binding(self.vm, |binding| {
            let value = self.state.with(|state| {
                let value = operation(state);
                super::set_line(binding, self.irq, state.irq());
                self.native_signal(state);
                value
            });
            binding.publish_changed_interrupts();
            value
        })
    }
    pub(super) fn close(&self) {
        if !self.installed.load(Ordering::Acquire) {
            self.state.with(|state| state.closed = true);
            return;
        }
        if self.mutate(|state| state.closed = true).is_err() {
            self.state.with(|state| {
                state.closed = true;
                self.native_signal(state);
            });
        }
    }
    pub(super) fn mmio(&self, access: MmioAccess) -> MmioAction {
        let offset = access.address().get() - self.base;
        let width = access.size();
        let operation = access.operation();
        self.mutate(|state| {
            let read = match (offset, width, operation) {
                (0x00, 4, MmioOperation::Read) => Some(0x4859_4d42),
                (0x04, 4, MmioOperation::Read) => Some(1),
                (0x08, 4, MmioOperation::Read) => Some(state.guest_status() as u64),
                (0x0c, 4, MmioOperation::Read) => Some(state.irq_mask as u64),
                (0x10, 4, MmioOperation::Read) => Some(state.outgoing.length as u64),
                (0x18, 8, MmioOperation::Read) => Some(state.outgoing.sequence),
                (0x100..=0x1fc, 4, MmioOperation::Read) if offset.is_multiple_of(4) => {
                    let index = (offset - 0x100) as usize;
                    let bytes = &state.outgoing.data[index..index + 4];
                    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64)
                }
                _ => None,
            };
            if let Some(value) = read {
                return MmioAction::CompleteRead(value);
            }
            let MmioOperation::Write(value) = operation else {
                return MmioAction::Stop;
            };
            let result = match (offset, width) {
                (0x08, 4) if value == model::ERROR as u64 => {
                    state.error = false;
                    Ok(())
                }
                (0x0c, 4) => {
                    state.irq_mask = value as u32 & 15;
                    Ok(())
                }
                (0x20, 8) => state.consume(value),
                (0x28, 4) if value <= model::RECORD_BYTES as u64 => {
                    state.staging_length = value as usize;
                    Ok(())
                }
                (0x2c, 4) if value == 1 => state.commit_guest(),
                (0x200..=0x2fc, 4) if offset.is_multiple_of(4) => {
                    let index = (offset - 0x200) as usize;
                    state.staging[index..index + 4].copy_from_slice(&(value as u32).to_le_bytes());
                    Ok(())
                }
                _ => Err(model::Error::Invalid),
            };
            if result.is_err() {
                state.error = true;
            }
            MmioAction::CompleteWrite
        })
        .unwrap_or(MmioAction::Stop)
    }
}

#[derive(Clone)]
pub(crate) struct Mailbox {
    shared: FallibleArc<Shared>,
}
impl Mailbox {
    pub(crate) fn prepare(
        machine: &FallibleArc<InstalledMachine>,
        base: u64,
        irq: u32,
        domain: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !super::valid_location(base, irq) {
            return Err(Error::InvalidArgument);
        }
        let vm = machine.io_install_id()?;
        let charge = super::charge::<Self, Shared>(domain)?;
        let shared = FallibleArc::try_new(Shared {
            vm,
            base,
            irq,
            state: InterruptSpinLock::new(model::MailboxState::new()),
            signals: SignalState::with_initial_level(SignalMask::from_trusted_bits(2)),
            installed: AtomicBool::new(false),
            _charge: charge,
        })
        .map_err(|_| Error::NoMemory)?;
        Ok(Self { shared })
    }
    pub(crate) fn install(&self, machine: &FallibleArc<InstalledMachine>) -> Result<(), Error> {
        machine.with_io_install(|vm| {
            if vm != self.shared.vm {
                return Err(Error::BadState);
            }
            let route = Route::Mailbox(self.shared.clone());
            machine.validate_io_route(&route)?;
            super::with_irq_binding(vm, |binding| {
                binding.preflight_io_route(&route)?;
                binding.install_io_route(route)?;
                self.shared.installed.store(true, Ordering::Release);
                Ok(())
            })?
        })
    }
    pub(crate) fn send(&self, bytes: &[u8]) -> Result<(), Error> {
        self.shared
            .mutate(|state| state.send(bytes))?
            .map_err(Into::into)
    }
    pub(crate) fn claim(&self) -> Result<Claim<'_>, Error> {
        let mut data = [0; model::RECORD_BYTES];
        let result = self.shared.state.with(|state| {
            let result = state.claim(&mut data);
            self.shared.native_signal(state);
            result
        })?;
        Ok(Claim {
            shared: &self.shared,
            data,
            length: result.0,
            sequence: result.1,
            resolved: false,
        })
    }
}

/// A failed user copy drops this claim without consuming its original record.
pub(crate) struct Claim<'a> {
    shared: &'a Shared,
    data: [u8; model::RECORD_BYTES],
    length: usize,
    sequence: u64,
    resolved: bool,
}
impl Claim<'_> {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data[..self.length]
    }
    pub(crate) fn commit(mut self) {
        let operation = |state: &mut model::MailboxState| state.finish_claim(self.sequence, true);
        if self.shared.mutate(operation).is_err() {
            self.shared.state.with(|state| {
                operation(state);
                self.shared.native_signal(state);
            });
        }
        self.resolved = true;
    }
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            // Aborting preserves the occupied guest-to-Native slot, so guest
            // IRQ level is unchanged. No registry or hardware work in Drop.
            self.shared.state.with(|state| {
                state.finish_claim(self.sequence, false);
                self.shared.native_signal(state);
            });
        }
    }
}

impl private::Sealed for Mailbox {}
impl private::UserExportable for Mailbox {}
impl KernelObject for Mailbox {
    const KIND: ObjectKind = ObjectKind::GUEST_MAILBOX;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::READ
        .union(Rights::WRITE)
        .union(Rights::WAIT)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.shared.signals,
            SignalMask::from_trusted_bits(7),
        ))
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.shared.close();
    }
}
