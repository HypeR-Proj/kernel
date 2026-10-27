// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::super::Profile;
use super::{PhysicalDevice, Route, State};
use crate::vm::registry::VmBinding;
use hyper_drivers::pci::Transport;
use {crate::irq::interrupt, crate::vm::io};
use {
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

impl PhysicalDevice {
    fn irq_write(&self, offset: usize) -> bool {
        matches!(self.claim.hardware.profile, Profile::Virtio(_)) && matches!(offset, 0x64 | 0x70)
    }

    pub(crate) fn access_at(&self, offset: usize, access: MmioAccess) -> MmioAction {
        let pci = matches!(self.claim.hardware.profile, Profile::Pci(_));
        if !(matches!(access.size(), 1 | 2 | 4) || (pci && access.size() == 8))
            || !offset.is_multiple_of(access.size())
            || (!pci
                && self
                    .claim
                    .hardware
                    .register(offset, access.size())
                    .is_none())
            || (matches!(self.claim.hardware.profile, Profile::Virtio(_))
                && offset < 0x100
                && access.size() != 4)
        {
            return MmioAction::Stop;
        }
        let route = self.state.with(|state| match state {
            State::Active(route) => Some(*route),
            _ => None,
        });
        let Some(route) = route else {
            return MmioAction::Stop;
        };
        let action = io::with_irq_binding(route.vm, |binding| {
            let action = self.state.with(|state| {
                let State::Active(active) = state else {
                    return MmioAction::Stop;
                };
                match &self.claim.hardware.profile {
                    Profile::Pci(transport) => {
                        self.access_pci(transport, binding, route.irq, offset, access)
                    }
                    Profile::Virtio(_) | Profile::Userspace => {
                        self.access_registers(active, route.irq, binding, offset, access)
                    }
                }
            });
            binding.publish_changed_interrupts();
            action
        })
        .unwrap_or(MmioAction::Stop);
        if ((matches!(access.operation(), MmioOperation::Write(_)) && self.irq_write(offset))
            || matches!(&self.claim.hardware.profile, Profile::Pci(t) if t.uses_intx()))
            && matches!(
                self.claim.hardware.trigger,
                hyper::hal::interrupt::InterruptTrigger::Level
            )
        {
            let enabled = self.registrations.with(|slots| {
                slots[0].as_ref().is_some_and(|registration| {
                    interrupt::enable_registered_shared(registration).is_ok()
                })
            });
            if !enabled {
                return MmioAction::Stop;
            }
        }
        action
    }

    // The caller holds state throughout the access; PCI shadow state nests
    // inside it, and saved interrupt mutation completes before either unlocks.
    fn access_pci(
        &self,
        transport: &Transport,
        binding: &VmBinding,
        irq: u32,
        offset: usize,
        access: MmioAccess,
    ) -> MmioAction {
        self.pci.with(|slot| {
            let Some(pci) = slot else {
                return MmioAction::Stop;
            };
            let action = match access.operation() {
                MmioOperation::Read => transport
                    .read(pci, offset, access.size())
                    .map(MmioAction::CompleteRead)
                    .unwrap_or(MmioAction::Stop),
                MmioOperation::Write(value) => {
                    if transport.write(pci, offset, access.size(), value) {
                        MmioAction::CompleteWrite
                    } else {
                        MmioAction::Stop
                    }
                }
            };
            if let Some(irq) = pci.take_pending_irq() {
                io::inject_interrupt(binding, irq);
            }
            if transport.uses_intx() {
                // Reading the virtio ISR can deassert INTx. Every access samples
                // the physical line before the masked host source is rearmed.
                io::set_line(binding, irq, transport.line_asserted());
            }
            action
        })
    }

    fn access_registers(
        &self,
        active: &mut Route,
        irq: u32,
        binding: &VmBinding,
        offset: usize,
        access: MmioAccess,
    ) -> MmioAction {
        let action = match access.operation() {
            MmioOperation::Read => self
                .claim
                .hardware
                .read_access(offset, access.size())
                .map(MmioAction::CompleteRead)
                .unwrap_or(MmioAction::Stop),
            MmioOperation::Write(value) => {
                if matches!(self.claim.hardware.profile, Profile::Virtio(_))
                    && !active.negotiation.write(offset, value as u32)
                {
                    return MmioAction::Stop;
                }
                if !self
                    .claim
                    .hardware
                    .write_access(offset, access.size(), value)
                {
                    return MmioAction::Stop;
                }
                MmioAction::CompleteWrite
            }
        };
        if matches!(access.operation(), MmioOperation::Write(_)) && self.irq_write(offset) {
            io::set_line(binding, irq, self.claim.hardware.line_asserted());
        }
        action
    }
}
