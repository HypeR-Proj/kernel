// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One reserved receive timer per UART, retired before device storage is freed.

use alloc::boxed::Box;
use hyper::mm::{FallibleArc, try_box};
use hyper::sync::{InterruptSpinLock, PublishedOnce};

use crate::vm::serial::Route;
use {crate::time::IrqCallout, crate::time::OwnedArmedIrqCallout};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Allocation,
    Timer,
    Unbound,
}

struct Control {
    armed: Option<OwnedArmedIrqCallout>,
    deadline: Option<u64>,
}

struct Context {
    route: PublishedOnce<Route>,
    expire: fn(Route),
}

pub(super) struct SerialTimeout {
    control: InterruptSpinLock<Control, crate::hal::irq::LocalMask>,
    reservation: FallibleArc<IrqCallout>,
    context: Box<Context>,
}

impl SerialTimeout {
    pub(super) fn try_new(expire: fn(Route)) -> Result<Self, Error> {
        let reservation = FallibleArc::try_new(IrqCallout::try_new().map_err(|_| Error::Timer)?)
            .map_err(|_| Error::Allocation)?;
        Ok(Self {
            control: InterruptSpinLock::new(Control {
                armed: None,
                deadline: None,
            }),
            reservation,
            context: try_box(Context {
                route: PublishedOnce::new(),
                expire,
            })
            .map_err(|_| Error::Allocation)?,
        })
    }

    pub(super) const fn allocation_bytes() -> usize {
        FallibleArc::<IrqCallout>::allocation_size()
            + IrqCallout::allocation_size()
            + core::mem::size_of::<Context>()
    }

    pub(super) fn bind(&self, route: Route) {
        if self.context.route.publish(route).is_err() {
            hyper::debug::invariant_failure("duplicate UART timer route");
        }
    }

    /// Read the deadline under timer control -> UART lock ordering. The callback
    /// takes only the UART lock and must never rearm this timer. Retire waits
    /// after releasing the UART lock so an in-flight callback can finish.
    pub(super) fn reschedule(&self, deadline: impl FnOnce() -> Option<u64>) -> Result<(), Error> {
        self.control.with(|control| {
            let deadline = deadline();
            if control.deadline == deadline {
                return Ok(());
            }
            if let Some(armed) = control.armed.take() {
                armed.retire().map_err(|_| Error::Timer)?;
            }
            control.deadline = None;
            if let Some(deadline) = deadline {
                if self.context.route.get().is_none() {
                    return Err(Error::Unbound);
                }
                let context = core::ptr::from_ref(&*self.context).expose_provenance();
                control.armed = Some(
                    IrqCallout::arm_owned(self.reservation.clone(), deadline, expire, context)
                        .map_err(|_| Error::Timer)?,
                );
                control.deadline = Some(deadline);
            }
            Ok(())
        })
    }

    /// Called after registry lookup is cut. Close UART admission before retiring
    /// the exact timer arm, including any callback currently using its context.
    pub(super) fn quiesce(&self, close: impl FnOnce()) -> Result<(), Error> {
        self.control.with(|control| {
            close();
            control.deadline = None;
            if let Some(armed) = control.armed.take() {
                armed.retire().map_err(|_| Error::Timer)?;
            }
            Ok(())
        })
    }
}

fn expire(context: usize) {
    // SAFETY: the owning device retains this stable allocation until quiesce
    // has retired the exact timer arm and all in-flight callback uses.
    let context = unsafe { &*core::ptr::with_exposed_provenance::<Context>(context) };
    if let Some(route) = context.route.get().copied() {
        (context.expire)(route);
    }
}
