// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! IRQ registrations have the same lifetime as the retained device owner.

use super::super::{Profile, model};
use super::{Error, PhysicalDevice, Route, State};
use core::sync::atomic::{AtomicPtr, Ordering};
use hyper::hal::interrupt::{InterruptId, InterruptPriority};
use {
    crate::irq::interrupt, crate::irq::interrupt::HandlerResult,
    crate::irq::interrupt::VirtualInterrupt,
};
use {crate::vm::io, crate::vm::registry::VmId};

pub(super) struct InterruptContext {
    owner: AtomicPtr<PhysicalDevice>,
    vector: usize,
}
impl InterruptContext {
    pub(super) const fn new(vector: usize) -> Self {
        Self {
            owner: AtomicPtr::new(core::ptr::null_mut()),
            vector,
        }
    }
}

impl PhysicalDevice {
    pub(crate) fn activate_for(&self, vm: VmId, irq: u32) -> Result<(), Error> {
        self.state.with(|state| {
            if !matches!(state, State::Attached) {
                return Err(Error::BadState);
            }
            *state = State::Active(Route {
                vm,
                irq,
                negotiation: model::Negotiation::new(),
                interrupt: model::LevelInterrupt::new(),
            });
            Ok(())
        })?;
        let hw = &self.claim.hardware;
        for vector in 0..hw.interrupt_count as usize {
            if let Err(error) = self.register_interrupt(vector) {
                // No guest has run and no PCI bus mastering was enabled. Undo
                // every prepared route before relinquishing the stable owner.
                self.state.with(|state| *state = State::Attached);
                self.unregister_interrupts()?;
                return Err(error);
            }
        }
        if let Profile::Pci(transport) = &hw.profile {
            self.state.with(|state| {
                if matches!(state, State::Active(_)) {
                    self.pci.with(|slot| {
                        if let Some(pci) = slot {
                            transport.activate(pci);
                        }
                    });
                }
            });
        }
        Ok(())
    }

    fn register_interrupt(&self, vector: usize) -> Result<(), Error> {
        let hw = &self.claim.hardware;
        let context = self.contexts.get(vector).ok_or(Error::InvalidArgument)?;
        // The object is already at its final KernelRef address. IRQ callbacks
        // retain this pointer only until synchronized unregister completes.
        context
            .owner
            .store(core::ptr::from_ref(self).cast_mut(), Ordering::Release);
        let prepared = hw
            .domain
            .prepare_shared_mapping(
                InterruptId::new(
                    hw.interrupt
                        .get()
                        .checked_add(vector as u32)
                        .ok_or(Error::Interrupt)?,
                ),
                InterruptPriority::Normal,
                hw.trigger,
                core::ptr::from_ref(context).expose_provenance(),
                interrupt_handler,
            )
            .map_err(|_| Error::Interrupt)?;
        let registration = match interrupt::activate(prepared) {
            Ok(registration) => registration,
            Err(failure) => {
                let (_, prepared) = failure.into_parts();
                if let Err(failure) = interrupt::discard_prepared(prepared) {
                    let _retained = core::mem::ManuallyDrop::new(failure);
                    crate::diagnostics::crash::fatal(format_args!(
                        "assigned-device IRQ activation rollback failed"
                    ));
                }
                return Err(Error::Interrupt);
            }
        };
        self.registrations
            .with(|slots| slots[vector] = Some(registration));
        Ok(())
    }

    fn unregister_interrupts(&self) -> Result<(), Error> {
        // IRQ callbacks take state, so unregister must run outside that lock.
        for vector in 0..self.contexts.len() {
            let Some(registration) = self.registrations.with(|slots| slots[vector].take()) else {
                continue;
            };
            let irq = registration.interrupt();
            if let Err(failure) = interrupt::unregister(registration) {
                let (_, registration) = failure.into_parts();
                self.registrations
                    .with(|slots| slots[vector] = Some(registration));
                self.state.with(|state| *state = State::Quarantined);
                return Err(Error::Quarantined);
            }
            if interrupt::unmap(irq).is_err() {
                self.state.with(|state| *state = State::Quarantined);
                return Err(Error::Quarantined);
            }
        }
        Ok(())
    }

    pub(crate) fn quiesce(&self) -> Result<(), Error> {
        self.state.with(|state| {
            match state {
                State::Retired => return Ok(()),
                State::Quarantined => return Err(Error::Quarantined),
                State::Claimed | State::Attached => {
                    *state = State::Retired;
                    return Ok(());
                }
                State::Active(_) => {}
            }
            // All owning vCPUs have detached. Require the transport's reset
            // acknowledgement; PCI BME/MSI gating alone cannot prove DMA drained.
            if let Profile::Pci(transport) = &self.claim.hardware.profile {
                let reset = transport.reset();
                self.pci.with(|slot| {
                    if let Some(pci) = slot {
                        transport.stop(pci);
                    }
                });
                if reset {
                    *state = State::Retired;
                    return Ok(());
                }
                *state = State::Quarantined;
                return Err(Error::Quarantined);
            }
            if matches!(self.claim.hardware.profile, Profile::Userspace) {
                *state = State::Quarantined;
                return Err(Error::Quarantined);
            }
            // A modern virtio reset acknowledges only after DMA has stopped.
            self.claim.hardware.write(0x70, 0);
            for _ in 0..1024 {
                if self.claim.hardware.read(0x70) == 0 {
                    *state = State::Retired;
                    return Ok(());
                }
                core::hint::spin_loop();
            }
            *state = State::Quarantined;
            Err(Error::Quarantined)
        })?;
        self.unregister_interrupts()
    }

    fn deliver_msi(&self, vector: usize) {
        let route = self.state.with(|state| match state {
            State::Active(route) => Some(*route),
            _ => None,
        });
        let Some(route) = route else {
            return;
        };
        // Before registry publication there can only be stale firmware edges:
        // guest MMIO and hence guest bus mastering are still inaccessible.
        let _ = io::with_irq_binding(route.vm, |binding| {
            self.state.with(|state| {
                if !matches!(state, State::Active(_)) {
                    return;
                }
                self.pci.with(|slot| {
                    if let Some(irq) = slot.as_ref().and_then(|pci| pci.delivered_irq(vector)) {
                        io::inject_interrupt(binding, irq);
                    }
                });
            });
            binding.publish_changed_interrupts();
        });
    }
}

fn interrupt_handler(
    _: VirtualInterrupt,
    context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    // SAFETY: the registration is owned by the final KernelRef allocation and
    // synchronized unregister precedes its release. Callbacks never acquire
    // registrations or mutate the IRQ registry.
    let context = unsafe { &*core::ptr::with_exposed_provenance::<InterruptContext>(context) };
    let pointer = context.owner.load(Ordering::Acquire);
    // SAFETY: register_interrupt publishes a non-null stable owner before the
    // handler is registered; the registration retains the enclosing context.
    let object = unsafe { &*pointer };
    if matches!(&object.claim.hardware.profile, Profile::Pci(t) if !t.uses_intx()) {
        object.deliver_msi(context.vector);
        return HandlerResult::Handled;
    }
    if matches!(object.claim.hardware.profile, Profile::Userspace) {
        object.state.with(|state| {
            if let State::Active(route) = state {
                if route.interrupt.deliver().is_err() {
                    crate::diagnostics::crash::fatal(format_args!("physical IRQ token exhausted"));
                }
                object.set_readable(route.interrupt.readable());
            }
        });
        return HandlerResult::HandledAndMaskLocal;
    }
    let route = object.state.with(|state| match state {
        State::Active(route) => Some(*route),
        _ => None,
    });
    let mask = if let Some(route) = route {
        io::with_irq_binding(route.vm, |binding| {
            let mask = object.state.with(|state| {
                let active = matches!(state, State::Active(_));
                let status = u32::from(object.claim.hardware.line_asserted());
                if active {
                    io::set_line(binding, route.irq, status != 0);
                }
                model::mask_interrupt(active, status, object.claim.hardware.trigger)
            });
            binding.publish_changed_interrupts();
            mask
        })
        .unwrap_or_else(|_| {
            // Before publication virtio remains reset. Preserve the trigger
            // decision so a stale edge cannot strand future completions.
            object.state.with(|state| {
                model::mask_interrupt(
                    matches!(state, State::Active(_)),
                    u32::from(object.claim.hardware.line_asserted()),
                    object.claim.hardware.trigger,
                )
            })
        })
    } else {
        true
    };
    if mask {
        HandlerResult::HandledAndMaskLocal
    } else {
        HandlerResult::Handled
    }
}
