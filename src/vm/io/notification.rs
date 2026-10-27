// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Direct queue kicks and completion IRQs, independent of userspace scheduling.

use super::super::{
    installed::InstalledMachine,
    registry::{VmBinding, VmId},
};
use super::{Error, Route, model};
use crate::authority::Rights;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalState,
    crate::object::TransferClass, crate::object::private,
};

static NEXT_ROUTE: AtomicU64 = AtomicU64::new(1);
fn new_route_id() -> Result<u64, Error> {
    NEXT_ROUTE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| Error::ResourceLimit)
}
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

pub(crate) struct Shared {
    pub(super) route_id: u64,
    front: Option<VmId>,
    back: VmId,
    pub(super) front_base: u64,
    frontend: AtomicU64,
    pub(super) back_base: u64,
    pub(super) front_irq: u32,
    pub(super) back_irq: u32,
    state: InterruptSpinLock<model::NotificationState, crate::hal::irq::LocalMask>,
    admission:
        InterruptSpinLock<super::super::memory::admission::Reply, crate::hal::irq::LocalMask>,
    installed: AtomicBool,
    detached: AtomicBool,
    signals: SignalState,
    _charge: CommittedCharge,
}

/// Retains each changed controller until its saved state has been published.
/// Keeping this obligation local to a mutation prevents a later no-op from
/// consuming it; publication must run after releasing the source lock.
#[must_use = "publish saved interrupt changes outside the source lock"]
struct PendingInterrupts {
    front: Option<VmBinding>,
    back: Option<VmBinding>,
}

impl PendingInterrupts {
    fn publish(self) {
        if let Some(front) = self.front {
            front.publish_changed_interrupts();
        }
        if let Some(back) = self.back {
            back.publish_changed_interrupts();
        }
    }
}

impl Shared {
    fn mutate<R>(&self, operation: impl FnOnce(&mut model::NotificationState) -> R) -> R {
        let (value, pending) = self.update_saved(operation);
        pending.publish();
        value
    }

    fn update_saved<R>(
        &self,
        operation: impl FnOnce(&mut model::NotificationState) -> R,
    ) -> (R, PendingInterrupts) {
        // Either peer can leave the registry independently. Keep the surviving
        // MMIO endpoint operational and let Native policy report device failure.
        let front = self
            .front
            .and_then(|id| super::super::registry::acquire_binding(id).ok());
        let back = super::super::registry::acquire_binding(self.back).ok();
        let (value, front_changed, back_changed) = self.state.with(|state| {
            if self.detached.load(Ordering::Acquire) {
                return (operation(state), false, false);
            }
            let previous_front = state.front_irq();
            let previous_back = state.back_irq();
            let previous_signals = state.signals(self.front.is_none());
            if (self.front.is_some() && front.is_none()) || back.is_none() {
                state.close();
            }
            let value = operation(state);
            let front_changed = previous_front != state.front_irq();
            let back_changed = previous_back != state.back_irq();
            // The source lock serializes both the state transition and its
            // saved-controller update. Repeated kicks/completions coalesce;
            // they do not require another controller lock or scheduler prompt.
            if front_changed && let Some(front) = &front {
                super::set_line(front, self.front_irq, state.front_irq());
            }
            if back_changed && let Some(back) = &back {
                super::set_line(back, self.back_irq, state.back_irq());
            }
            let signals = state.signals(self.front.is_none());
            if previous_signals != signals
                && self
                    .signals
                    .update(
                        SignalMask::from_trusted_bits(7),
                        SignalMask::from_trusted_bits(signals),
                    )
                    .is_err()
            {
                hyper::debug::invariant_failure("vm::io::notification::mutate invariant");
            }
            (value, front_changed, back_changed)
        });
        (
            value,
            PendingInterrupts {
                front: if front_changed { front } else { None },
                back: if back_changed { back } else { None },
            },
        )
    }
    pub(super) fn close(&self) {
        if !self.installed.load(Ordering::Acquire) {
            self.state.with(model::NotificationState::close);
            return;
        }
        self.mutate(model::NotificationState::close);
    }
    pub(super) fn frontend_base(&self) -> u64 {
        self.frontend.load(Ordering::Acquire) & !0xfff
    }

    pub(super) fn frontend_decode_enabled(&self) -> bool {
        let mapping = self.frontend.load(Ordering::Acquire);
        mapping & 1 == 0 || mapping & 2 != 0
    }

    pub(super) fn configure_frontend(&self, base: u64, flags: u32) -> Result<(), Error> {
        // Called under the frontend VM lifecycle admission lock, after checking
        // its userspace MMIO reservation and all sibling route reservations.
        self.mutate(|state| {
            if state.closed || self.detached.load(Ordering::Acquire) {
                return Err(Error::BadState);
            }
            self.frontend
                .store(base | u64::from(flags), Ordering::Release);
            state.interrupt_masked = flags & 4 != 0;
            Ok(())
        })
    }

    /// Userspace transport fallback (for example PCI configuration access).
    /// This reaches only the admitted frontend, never backend control or DMA.
    pub(super) fn frontend_access(&self, operation: u32, value: u32) -> Result<u32, Error> {
        if operation > 2 || (operation != 2 && value != 0) || (operation == 2 && value >= 32) {
            return Err(Error::InvalidArgument);
        }
        self.mutate(|state| {
            if state.closed || self.detached.load(Ordering::Acquire) {
                return Err(Error::BadState);
            }
            match operation {
                0 => Ok(state.status),
                1 => {
                    let status = state.status;
                    state.ack(status);
                    Ok(status)
                }
                _ => {
                    let mapping = self.frontend.load(Ordering::Acquire);
                    if mapping & 1 == 0 || mapping & 8 != 0 {
                        state.kick(u64::from(value));
                    }
                    Ok(0)
                }
            }
        })
    }

    pub(super) fn front_mmio(&self, access: MmioAccess) -> Option<MmioAction> {
        let offset = access.address().get().checked_sub(self.frontend_base())?;
        if !matches!(offset, 0x50 | 0x60 | 0x64) {
            return None;
        }
        if offset == 0x60 && access.size() == 4 && matches!(access.operation(), MmioOperation::Read)
        {
            // Preserve MMIO's read-only fast path. Unlike PCI ISR, this read
            // neither acknowledges the interrupt nor needs registry leases.
            return self.state.with(|state| {
                let mapping = self.frontend.load(Ordering::Acquire);
                (mapping & 1 == 0 && access.address().get() == (mapping & !0xfff) + 0x60)
                    .then_some(MmioAction::CompleteRead(u64::from(state.status)))
            });
        }
        self.mutate(|state| {
            // Recheck under the source lock: a cloned route may predate a BAR
            // relocation or disabling memory decode. Such accesses must not
            // reach the old doorbell or consume interrupt status.
            let mapping = self.frontend.load(Ordering::Acquire);
            let offset = access.address().get().checked_sub(mapping & !0xfff)?;
            let pci = mapping & 1 != 0;
            if pci && mapping & 2 == 0 {
                return None;
            }
            match (offset, access.size(), access.operation()) {
                (0x50, size, MmioOperation::Write(queue)) if size == if pci { 2 } else { 4 } => {
                    if !pci || mapping & 8 != 0 {
                        state.kick(queue);
                    }
                    Some(MmioAction::CompleteWrite)
                }
                (0x60, 1, MmioOperation::Read) if pci => {
                    let status = state.status;
                    state.ack(status);
                    Some(MmioAction::CompleteRead(u64::from(status)))
                }
                (0x60, 4, MmioOperation::Read) if !pci => {
                    Some(MmioAction::CompleteRead(u64::from(state.status)))
                }
                (0x64, 4, MmioOperation::Write(mask)) if !pci => {
                    state.ack(mask as u32);
                    Some(MmioAction::CompleteWrite)
                }
                _ => None,
            }
        })
    }
    pub(super) fn back_mmio(&self, access: MmioAccess) -> MmioAction {
        let offset = access.address().get() - self.back_base;
        if access.size() == 4 && matches!(access.operation(), MmioOperation::Read) {
            match offset {
                0x00 => return MmioAction::CompleteRead(0x4859_4e42),
                0x04 => return MmioAction::CompleteRead(1),
                0x08 => {
                    return self
                        .state
                        .with(|state| MmioAction::CompleteRead(state.epoch as u64));
                }
                0x0c => {
                    return self
                        .state
                        .with(|state| MmioAction::CompleteRead(u64::from(state.enabled)));
                }
                _ => {}
            }
        }
        if (0x20..=0x70).contains(&offset) {
            if access.size() != 8 {
                return MmioAction::Stop;
            }
            return self.admission.with(|reply| {
                if self.detached.load(Ordering::Acquire) {
                    return MmioAction::Stop;
                }
                match (offset, access.operation()) {
                    (0x20, MmioOperation::Write(token)) => {
                        reply.quiesce(self.back, self.route_id, token);
                        MmioAction::CompleteWrite
                    }
                    (0x28, MmioOperation::Write(token)) => {
                        reply.admit(self.back, self.route_id, token);
                        MmioAction::CompleteWrite
                    }
                    (0x50, MmioOperation::Write(index)) => {
                        reply.select(self.back, self.route_id, index);
                        MmioAction::CompleteWrite
                    }
                    (offset, MmioOperation::Read) => reply
                        .read(offset)
                        .map_or(MmioAction::Stop, MmioAction::CompleteRead),
                    _ => MmioAction::Stop,
                }
            });
        }

        self.mutate(|state| match (offset, access.size(), access.operation()) {
            (0x10, 4, MmioOperation::Read) => MmioAction::CompleteRead(state.take_kicks() as u64),
            (0x18, 8, MmioOperation::Write(value)) => {
                state.call(value);
                MmioAction::CompleteWrite
            }
            _ => MmioAction::Stop,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Notification {
    shared: FallibleArc<Shared>,
}
impl Notification {
    #[cfg(all(
        CONFIG_ARCH_AARCH64,
        all(feature = "kernel-test", feature = "kernel-self-test")
    ))]
    pub(crate) fn verify_delivery_for_test(
        front: super::super::registry::PreparedVm,
        back: super::super::registry::PreparedVm,
        domain: &ResourceDomain,
    ) -> Result<(), &'static str> {
        delivery_test::run(front, back, domain)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        front: &FallibleArc<InstalledMachine>,
        back: &FallibleArc<InstalledMachine>,
        front_base: u64,
        back_base: u64,
        front_irq: u32,
        back_irq: u32,
        domain: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !super::valid_location(front_base, front_irq)
            || !super::valid_location(back_base, back_irq)
        {
            return Err(Error::InvalidArgument);
        }
        let front = front.io_install_id()?;
        let back = back.io_update_id()?;
        if front == back {
            return Err(Error::InvalidArgument);
        }
        let charge = super::charge::<Self, Shared>(domain)?;
        let shared = FallibleArc::try_new(Shared {
            route_id: new_route_id()?,
            front: Some(front),
            back,
            front_base,
            frontend: AtomicU64::new(front_base),
            back_base,
            front_irq,
            back_irq,
            state: InterruptSpinLock::new(model::NotificationState::new()),
            admission: InterruptSpinLock::new(super::super::memory::admission::Reply::new()),
            installed: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            signals: SignalState::new(),
            _charge: charge,
        })
        .map_err(|_| Error::NoMemory)?;
        Ok(Self { shared })
    }
    pub(crate) fn install(
        &self,
        front: &FallibleArc<InstalledMachine>,
        back: &FallibleArc<InstalledMachine>,
    ) -> Result<(), Error> {
        let commit = || {
            let front_route = Route::NotificationFront(self.shared.clone());
            let back_route = Route::NotificationBack(self.shared.clone());
            front.validate_io_route(&front_route)?;
            back.validate_io_route(&back_route)?;
            super::with_irq_binding(self.shared.front.ok_or(Error::BadState)?, |front_binding| {
                super::with_irq_binding(self.shared.back, |back_binding| {
                    front_binding.preflight_io_route(&front_route)?;
                    back_binding.preflight_io_route(&back_route)?;
                    // Both VM lifecycle locks remain held in identity order.
                    // All fallible validation preceded this two-owner commit.
                    if front_binding.install_io_route(front_route).is_err()
                        || back_binding.install_io_route(back_route).is_err()
                    {
                        crate::diagnostics::crash::fatal(format_args!(
                            "guest notification reservation lost during commit"
                        ));
                    }
                    self.shared.installed.store(true, Ordering::Release);
                    Ok(())
                })?
            })?
        };
        if self.shared.front.ok_or(Error::BadState)? < self.shared.back {
            front.with_io_install(|id| {
                if Some(id) != self.shared.front {
                    return Err(Error::BadState);
                }
                back.with_io_update(|id| {
                    if id != self.shared.back {
                        return Err(Error::BadState);
                    }
                    commit()
                })
            })
        } else {
            back.with_io_update(|id| {
                if id != self.shared.back {
                    return Err(Error::BadState);
                }
                front.with_io_install(|id| {
                    if Some(id) != self.shared.front {
                        return Err(Error::BadState);
                    }
                    commit()
                })
            })
        }
    }
    /// A Native client uses the same backend register contract, with durable
    /// scheduler signals replacing the frontend guest interrupt line.
    pub(crate) fn prepare_native(
        back: &FallibleArc<InstalledMachine>,
        back_base: u64,
        back_irq: u32,
        domain: &ResourceDomain,
    ) -> Result<Self, Error> {
        if !super::valid_location(back_base, back_irq) {
            return Err(Error::InvalidArgument);
        }
        let shared = FallibleArc::try_new(Shared {
            route_id: new_route_id()?,
            front: None,
            back: back.io_install_id()?,
            front_base: 0,
            frontend: AtomicU64::new(0),
            back_base,
            front_irq: 0,
            back_irq,
            state: InterruptSpinLock::new(model::NotificationState::new()),
            admission: InterruptSpinLock::new(super::super::memory::admission::Reply::new()),
            installed: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            signals: SignalState::new(),
            _charge: super::charge::<Self, Shared>(domain)?,
        })
        .map_err(|_| Error::NoMemory)?;
        Ok(Self { shared })
    }

    pub(crate) fn install_native(&self, back: &FallibleArc<InstalledMachine>) -> Result<(), Error> {
        if self.shared.front.is_some() {
            return Err(Error::BadState);
        }
        back.with_io_install(|id| {
            if id != self.shared.back {
                return Err(Error::BadState);
            }
            let route = Route::NotificationBack(self.shared.clone());
            back.validate_io_route(&route)?;
            super::with_irq_binding(id, |binding| {
                binding.preflight_io_route(&route)?;
                binding.install_io_route(route)?;
                self.shared.installed.store(true, Ordering::Release);
                Ok(())
            })?
        })
    }

    pub(crate) fn kick_native_mask(&self, queues: u32) -> Result<(), Error> {
        if queues == 0 || queues & !((1 << model::QUEUE_COUNT) - 1) != 0 {
            return Err(Error::BadState);
        }
        self.shared.mutate(|state| {
            if state.closed || !state.enabled {
                return Err(Error::BadState);
            }
            for queue in 0..model::QUEUE_COUNT {
                if queues & (1 << queue) != 0 {
                    state.kick(queue);
                }
            }
            Ok(())
        })
    }

    pub(crate) fn acknowledge_native(&self) {
        self.shared.mutate(|state| state.ack(1));
    }

    pub(crate) fn native_signal_source(&self) -> SignalSource<'_> {
        SignalSource::new(&self.shared.signals, SignalMask::from_trusted_bits(7))
    }

    pub(crate) fn closed_signal_source(&self) -> SignalSource<'_> {
        SignalSource::new(&self.shared.signals, SignalMask::from_trusted_bits(1))
    }

    pub(crate) fn close_native(&self) {
        self.shared.close();
    }

    fn disconnect(&self) -> Result<u32, Error> {
        let front = self
            .shared
            .front
            .and_then(|id| super::super::registry::acquire_binding(id).ok());
        let back = super::super::registry::acquire_binding(self.shared.back).ok();
        // Admission is serialized with permanent withdrawal. Once detached,
        // even an already cloned old route can never affect a new binding.
        let detached = self.shared.admission.with(|_| {
            if self.shared.detached.load(Ordering::Acquire) {
                return Err(Error::BadState);
            }
            if back.as_ref().is_some_and(|binding| {
                binding.with_address_space(|space| {
                    space
                        .live
                        .slots
                        .iter()
                        .flatten()
                        .any(|record| record.state.owns(self.shared.route_id))
                })
            }) {
                return Err(Error::Busy);
            }
            self.shared.state.with(|state| {
                state.close();
                if self
                    .shared
                    .signals
                    .update(
                        SignalMask::from_trusted_bits(7),
                        SignalMask::from_trusted_bits(1),
                    )
                    .is_err()
                {
                    hyper::debug::invariant_failure("vm::io::notification::disconnect invariant");
                }
                if let Some(front) = &front {
                    super::set_line(front, self.shared.front_irq, false);
                }
                if let Some(back) = &back {
                    super::set_line(back, self.shared.back_irq, false);
                }
                self.shared.detached.store(true, Ordering::Release);
                let removed_front = front
                    .as_ref()
                    .and_then(|binding| binding.remove_io_notification(self.shared.route_id));
                let removed_back = back
                    .as_ref()
                    .and_then(|binding| binding.remove_io_notification(self.shared.route_id));
                Ok((state.epoch, removed_front, removed_back))
            })
        })?;
        if let Some(front) = front {
            front.publish_changed_interrupts();
        }
        if let Some(back) = back {
            back.publish_changed_interrupts();
        }
        let (epoch, front_route, back_route) = detached;
        drop(front_route);
        drop(back_route);
        Ok(epoch)
    }

    pub(crate) fn control(&self, operation: u32) -> Result<u32, Error> {
        if operation == hyper::abi::native::HYPER_NATIVE_GUEST_NOTIFICATION_DISCONNECT as u32 {
            return self.disconnect();
        }
        self.shared
            .mutate(|state| state.control(operation))
            .map_err(Into::into)
    }
}

#[cfg(all(
    CONFIG_ARCH_AARCH64,
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
#[path = "../../../tests/kernel/guest_notification.rs"]
mod delivery_test;
impl private::Sealed for Notification {}
impl private::UserExportable for Notification {}
impl KernelObject for Notification {
    const KIND: ObjectKind = ObjectKind::GUEST_NOTIFICATION;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::WRITE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::WAIT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.shared.signals,
            SignalMask::from_trusted_bits(1),
        ))
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.shared.close();
    }
}
