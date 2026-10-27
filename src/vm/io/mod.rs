// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VM-owned control mailboxes and prevalidated cross-VM notification routes.

mod interrupt;
mod mailbox;
pub(crate) use interrupt::Interrupt;
mod model;
mod notification;
pub(crate) mod service;
pub(crate) use mailbox::Mailbox;
pub(crate) use notification::Notification;

use super::registry::{VmBinding, VmId};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {crate::object::KernelObject, crate::object::object_allocation_size};
use {hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction};

pub(crate) use super::service::Error;

impl From<model::Error> for Error {
    fn from(value: model::Error) -> Self {
        match value {
            model::Error::Invalid => Self::InvalidArgument,
            model::Error::Busy => Self::WouldBlock,
            model::Error::Closed => Self::BadState,
            model::Error::Exhausted => Self::ResourceLimit,
        }
    }
}

fn charge<T: KernelObject, S>(domain: &ResourceDomain) -> Result<CommittedCharge, Error> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| value.checked_add(FallibleArc::<S>::allocation_size()))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(Error::NoMemory)?;
    domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelMemoryBytes, bytes)
                .with(ResourceKind::KernelObjects, 1),
        )
        .map(|value| value.commit())
        .map_err(|_| Error::ResourceLimit)
}

pub(super) fn valid_location(base: u64, irq: u32) -> bool {
    (0x0a00_0000..0x0b00_0000).contains(&base) && base.is_multiple_of(4096) && valid_irq(irq)
}

pub(super) fn valid_irq(irq: u32) -> bool {
    use hyper::abi::native as abi;
    crate::hal::vm::supports_io_notifications()
        && (abi::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_DEVICE_INTERRUPT_BASE as u32
            ..abi::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_INTERRUPT_COUNT as u32)
            .contains(&irq)
}

/// Mutation-only saved-model update. The caller serializes its source level
/// and calls `publish_changed_interrupts` after releasing the source lock.
pub(crate) fn set_line(binding: &VmBinding, irq: u32, asserted: bool) {
    if crate::hal::vm::update_saved_device_line(binding.interrupts(), irq, asserted).is_err() {
        crate::diagnostics::crash::fatal(format_args!(
            "validated guest notification IRQ became invalid"
        ));
    }
}

/// Mutation-only MSI event update. The caller calls `publish_changed_interrupts`
/// after releasing the source locks so remote vCPUs reconcile the saved state.
pub(crate) fn inject_interrupt(binding: &VmBinding, irq: u32) {
    if crate::hal::vm::inject_saved_device_interrupt(binding.interrupts(), irq).is_err() {
        crate::diagnostics::crash::fatal(format_args!("validated guest MSI became invalid"));
    }
}

/// Temporary generation-qualified binding; no route owns a registry VM lease.
pub(crate) fn with_irq_binding<R>(
    id: VmId,
    operation: impl FnOnce(&VmBinding) -> R,
) -> Result<R, Error> {
    super::registry::with_binding(id, operation).map_err(|_| Error::BadState)
}

#[derive(Clone)]
pub(crate) enum Route {
    Interrupt(FallibleArc<interrupt::Shared>),
    Mailbox(FallibleArc<mailbox::Shared>),
    NotificationFront(FallibleArc<notification::Shared>),
    NotificationBack(FallibleArc<notification::Shared>),
}
impl Route {
    pub(super) fn base(&self) -> Option<u64> {
        match self {
            Self::Interrupt(_) => None,
            Self::Mailbox(value) => Some(value.base),
            Self::NotificationFront(value) => Some(value.frontend_base()),
            Self::NotificationBack(value) => Some(value.back_base),
        }
    }
    fn decode_enabled(&self) -> bool {
        match self {
            Self::NotificationFront(value) => value.frontend_decode_enabled(),
            _ => true,
        }
    }
    fn same_frontend_identity(&self, other: &Self) -> bool {
        matches!((self, other), (Self::NotificationFront(left), Self::NotificationFront(right)) if left.front_base == right.front_base)
    }
    pub(crate) fn irq(&self) -> u32 {
        match self {
            Self::Interrupt(value) => value.irq,
            Self::Mailbox(value) => value.irq,
            Self::NotificationFront(value) => value.front_irq,
            Self::NotificationBack(value) => value.back_irq,
        }
    }
    pub(super) fn deferred_overlay(&self) -> bool {
        matches!(self, Self::NotificationFront(_))
    }
    fn route(&self, access: MmioAccess) -> Option<MmioAction> {
        match self {
            Self::Interrupt(_) => None,
            Self::Mailbox(value) => Some(value.mmio(access)),
            Self::NotificationFront(value) => value.front_mmio(access),
            Self::NotificationBack(value) => Some(value.back_mmio(access)),
        }
    }
    fn close(&self) {
        match self {
            Self::Interrupt(value) => value.close(),
            Self::Mailbox(value) => value.close(),
            Self::NotificationFront(value) | Self::NotificationBack(value) => value.close(),
        }
    }
}

pub(crate) struct Routes {
    entries: InterruptSpinLock<
        [Option<Route>; hyper::abi::native::HYPER_NATIVE_IO_MAX_CLIENTS as usize * 3],
        crate::hal::irq::LocalMask,
    >,
}
impl Routes {
    pub(crate) fn new() -> Self {
        Self {
            entries: InterruptSpinLock::new(core::array::from_fn(|_| None)),
        }
    }
    pub(crate) fn can_insert(&self, route: &Route) -> Result<(), Error> {
        self.entries.with(|entries| {
            if entries.iter().flatten().any(|other| {
                (route.base().is_some() && other.base() == route.base())
                    || other.irq() == route.irq()
                    || other.same_frontend_identity(route)
            }) || entries.iter().all(Option::is_some)
            {
                Err(Error::Busy)
            } else {
                Ok(())
            }
        })
    }
    /// VM lifecycle admission holds across preflight and this infallible commit.
    pub(crate) fn insert(&self, route: Route) -> Result<(), Error> {
        self.entries.with(|entries| {
            if entries.iter().flatten().any(|other| {
                (route.base().is_some() && other.base() == route.base())
                    || other.irq() == route.irq()
                    || other.same_frontend_identity(&route)
            }) {
                return Err(Error::Busy);
            }
            let slot = entries
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(Error::Busy)?;
            *slot = Some(route);
            Ok(())
        })
    }
    pub(crate) fn access_frontend(
        &self,
        original: u64,
        operation: u32,
        value: u32,
    ) -> Result<u32, Error> {
        let shared = self
            .entries
            .with(|entries| {
                entries.iter().flatten().find_map(|route| match route {
                    Route::NotificationFront(shared) if shared.front_base == original => {
                        Some(shared.clone())
                    }
                    _ => None,
                })
            })
            .ok_or(Error::BadState)?;
        // Drop the route lock before mutating the source/controller.
        shared.frontend_access(operation, value)
    }

    pub(crate) fn configure_frontend(
        &self,
        original: u64,
        base: u64,
        flags: u32,
    ) -> Result<(), Error> {
        let shared = self.entries.with(|entries| {
            let shared = entries
                .iter()
                .flatten()
                .find_map(|route| match route {
                    Route::NotificationFront(shared) if shared.front_base == original => {
                        Some(shared.clone())
                    }
                    _ => None,
                })
                .ok_or(Error::BadState)?;
            if !valid_location(base, shared.front_irq)
                || flags & !15 != 0
                || (flags & 1 == 0 && flags != 0)
            {
                return Err(Error::InvalidArgument);
            }
            // Resource reassignment can temporarily give disabled functions
            // the same BAR. Only simultaneous decoding would be ambiguous.
            let enabled = flags & 1 == 0 || flags & 2 != 0;
            if enabled
                && entries.iter().flatten().any(|route| {
                    let same = matches!(route, Route::NotificationFront(value)
                    if value.route_id == shared.route_id);
                    !same && route.decode_enabled() && route.base() == Some(base)
                })
            {
                return Err(Error::InvalidArgument);
            }
            Ok(shared)
        })?;
        // No route lock across source/controller mutation (disconnect takes the
        // reverse ownership path). Lifecycle admission excludes installations
        // and other relocations; concurrent removal only frees reservations.
        shared.configure_frontend(base, flags)
    }

    pub(crate) fn remove_notification(&self, route_id: u64) -> Option<Route> {
        self.entries.with(|entries| {
            entries
                .iter_mut()
                .find(|entry| match entry {
                    Some(Route::NotificationFront(shared) | Route::NotificationBack(shared)) => {
                        shared.route_id == route_id
                    }
                    _ => false,
                })
                .and_then(Option::take)
        })
    }

    pub(crate) fn conflicts(&self, base: u64, length: u64) -> bool {
        self.entries.with(|entries| {
            entries.iter().flatten().any(|route| {
                route.base().is_some_and(|start| {
                    base < start + 4096
                        && start < base.saturating_add(length)
                        && !(route.deferred_overlay()
                            && base <= start
                            && base.saturating_add(length) >= start + 4096)
                })
            })
        })
    }
    pub(crate) fn route(&self, access: MmioAccess) -> Option<MmioAction> {
        let address = access.address().get();
        let route = self.entries.with(|entries| {
            entries
                .iter()
                .flatten()
                .find(|route| {
                    route.decode_enabled()
                        && route
                            .base()
                            .is_some_and(|base| address >= base && address < base + 4096)
                })
                .cloned()
        })?;
        route.route(access)
    }
    pub(crate) fn close(&self) {
        // Release the registry's retained routes even when diagnostic Native
        // handles remain. No callback or destructor runs under the route lock.
        let detached = self
            .entries
            .with(|entries| core::mem::replace(entries, core::array::from_fn(|_| None)));
        for route in detached.into_iter().flatten() {
            route.close();
        }
    }
}
