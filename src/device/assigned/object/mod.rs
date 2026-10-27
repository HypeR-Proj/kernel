// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use alloc::boxed::Box;

use super::{Claim, Error};
use crate::authority::Rights;
use crate::vm::registry::VmId;
use hyper::sync::InterruptSpinLock;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {crate::irq::interrupt, crate::irq::interrupt::Registration};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::SignalMask,
    crate::object::SignalSource, crate::object::TransferClass, crate::object::private,
};
use {hyper_drivers::pci::FunctionState, hyper_drivers::pci::MAX_VECTORS};

mod access;
mod assignment;
mod interrupts;
mod metadata;
pub(crate) use assignment::Assignment;

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

fn charge<T: KernelObject>(domain: &ResourceDomain) -> Result<CommittedCharge, Error> {
    let bytes = crate::object::object_allocation_size::<T>().ok_or(Error::Resource)?;
    domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes as u64),
        )
        .map(|charge| charge.commit())
        .map_err(|_| Error::Resource)
}

pub(crate) struct DeviceAssignmentAuthority {
    _charge: CommittedCharge,
}
impl DeviceAssignmentAuthority {
    #[cfg_attr(
        all(feature = "kernel-test", feature = "kernel-self-test"),
        allow(dead_code)
    )]
    pub(crate) fn try_new(domain: &ResourceDomain) -> Result<Self, Error> {
        Ok(Self {
            _charge: charge::<Self>(domain)?,
        })
    }
}
impl private::Sealed for DeviceAssignmentAuthority {}
impl private::UserExportable for DeviceAssignmentAuthority {}
impl KernelObject for DeviceAssignmentAuthority {
    const KIND: ObjectKind = ObjectKind::DEVICE_ASSIGNMENT_AUTHORITY;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::DUPLICATE)
        .union(Rights::INSPECT)
        .union(Rights::ASSIGN_DEVICE)
        .union(Rights::MAP_DMA);
}

#[derive(Clone, Copy)]
struct Route {
    vm: VmId,
    irq: u32,
    negotiation: super::model::Negotiation,
    interrupt: super::model::LevelInterrupt,
}
enum State {
    Claimed,
    Attached,
    Active(Route),
    Retired,
    Quarantined,
}

pub(crate) struct PhysicalDevice {
    claim: Claim,
    state: Lock<State>,
    registrations: Lock<[Option<Registration>; MAX_VECTORS]>,
    contexts: [interrupts::InterruptContext; MAX_VECTORS],
    pci: Lock<Option<FunctionState>>,
    pub(crate) events: hyper::mm::FallibleArc<super::events::Events>,
    _charge: CommittedCharge,
}
impl PhysicalDevice {
    pub(crate) fn claim(index: usize, domain: &ResourceDomain) -> Result<Box<Self>, Error> {
        if !crate::hal::vm::supports_guest_device_assignment() {
            return Err(Error::Unsupported);
        }
        let charge = charge::<Self>(domain)?;
        let claim = super::super::platform_bus::claim(index).ok_or(Error::BadState)?;
        Self::try_claimed(claim, charge, domain)
    }
    pub(crate) fn claim_matching(
        profile: u32,
        identity_kind: u32,
        identity: &str,
        domain: &ResourceDomain,
    ) -> Result<Box<Self>, super::service::MatchError> {
        let charge = charge::<Self>(domain).map_err(super::service::classify)?;
        let claim = super::super::platform_bus::claim_matching(profile, identity_kind, identity)?;
        Self::try_claimed(claim, charge, domain)
            .map_err(|error| super::service::classify(error).into())
    }
    pub(crate) fn claim_bundle(
        entries: &[(u32, u32, u64)],
        irq_node: u32,
        domain: &ResourceDomain,
    ) -> Result<Box<Self>, Error> {
        if !crate::hal::vm::supports_guest_device_assignment() {
            return Err(Error::Unsupported);
        }
        let charge = charge::<Self>(domain)?;
        let claim = super::super::platform_bus::claim_bundle(entries, irq_node)?;
        Self::try_claimed(claim, charge, domain)
    }

    /// Keep the register and interrupt arrays in their final object storage.
    /// Both the physical claim and its charge roll back if allocation fails.
    fn try_claimed(
        claim: Claim,
        charge: CommittedCharge,
        domain: &ResourceDomain,
    ) -> Result<Box<Self>, Error> {
        let events = super::events::Events::new(domain)?;
        let mut storage = hyper::mm::try_box_uninit::<Self>().map_err(|_| Error::Resource)?;
        let target = storage.as_mut_ptr();
        // SAFETY: storage uniquely owns an aligned, uninitialized Self. Every
        // field is initialized exactly once, with no fallible operation after
        // moving the claim. Interrupt contexts are written individually to
        // avoid full-device and context-array temporaries on the syscall stack.
        unsafe {
            core::ptr::addr_of_mut!((*target).claim).write(claim);
            core::ptr::addr_of_mut!((*target).state).write(Lock::new(State::Claimed));
            core::ptr::addr_of_mut!((*target).registrations)
                .write(Lock::new([const { None }; MAX_VECTORS]));
            let contexts =
                core::ptr::addr_of_mut!((*target).contexts).cast::<interrupts::InterruptContext>();
            for index in 0..MAX_VECTORS {
                contexts
                    .add(index)
                    .write(interrupts::InterruptContext::new(index));
            }
            core::ptr::addr_of_mut!((*target).pci).write(Lock::new(None));
            core::ptr::addr_of_mut!((*target).events).write(events);
            core::ptr::addr_of_mut!((*target)._charge).write(charge);
            Ok(storage.assume_init())
        }
    }
    fn guest_interrupt_count(&self) -> u32 {
        self.claim.hardware.interrupt_count.max(1)
    }
    fn attach(&self, base: u64, irq: u32) -> Result<(), Error> {
        self.state.with(|state| match state {
            State::Claimed => {
                if let super::Profile::Pci(transport) = &self.claim.hardware.profile {
                    self.pci
                        .with(|slot| *slot = Some(transport.state(base, irq)));
                }
                *state = State::Attached;
                Ok(())
            }
            _ => Err(Error::BadState),
        })
    }
    fn set_readable(&self, ready: bool) {
        if self
            .events
            .signals
            .update(
                SignalMask::from_trusted_bits(1),
                SignalMask::from_trusted_bits(u64::from(ready)),
            )
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "physical IRQ signal sequence exhausted"
            ));
        }
    }
    pub(crate) fn mmio(
        &self,
        offset: u64,
        width: u32,
        write: bool,
        value: u64,
    ) -> Result<u64, Error> {
        let offset = usize::try_from(offset).map_err(|_| Error::InvalidArgument)?;
        let width = width as usize;
        if !matches!(self.claim.hardware.profile, super::Profile::Userspace) {
            return Err(Error::Unsupported);
        }
        if !matches!(width, 1 | 2 | 4)
            || !offset.is_multiple_of(width)
            || (write && value > (u64::MAX >> (64 - width * 8)))
            || (!write && value != 0)
        {
            return Err(Error::InvalidArgument);
        }
        // Even a device read may have side effects. Access begins only after
        // the installed VM owns all DMA backing; CPU access and retirement are
        // serialized here. Userspace selects register semantics, never extents.
        let route = self.state.with(|state| match state {
            State::Active(route) => Ok(*route),
            _ => Err(Error::BadState),
        })?;
        // Activation prepares the IRQ before registry publication. An active
        // object alone is not yet permission to touch hardware: reject access
        // until the fully installed VM and its retained backing are visible.
        crate::vm::io::with_irq_binding(route.vm, |_| {
            self.state.with(|state| {
                if !matches!(state, State::Active(_)) {
                    return Err(Error::BadState);
                }
                if write {
                    self.claim
                        .hardware
                        .write_access(offset, width, value)
                        .then_some(0)
                        .ok_or(Error::InvalidArgument)
                } else {
                    self.claim
                        .hardware
                        .read_access(offset, width)
                        .ok_or(Error::InvalidArgument)
                }
            })
        })
        .map_err(|_| Error::BadState)?
    }
    pub(crate) fn irq_pending(&self) -> Result<u64, Error> {
        if !matches!(self.claim.hardware.profile, super::Profile::Userspace) {
            return Err(Error::Unsupported);
        }
        self.state.with(|state| match state {
            State::Active(route) => Ok(route.interrupt.pending()),
            _ => Err(Error::BadState),
        })
    }
    pub(crate) fn irq_complete(&self, sequence: u64, asserted: bool) -> Result<(), Error> {
        if !matches!(self.claim.hardware.profile, super::Profile::Userspace) {
            return Err(Error::Unsupported);
        }
        let route = self.state.with(|state| match state {
            State::Active(route) => Ok(*route),
            _ => Err(Error::BadState),
        })?;
        crate::vm::io::with_irq_binding(route.vm, |binding| {
            let result = self.state.with(|state| {
                let State::Active(current) = state else {
                    return Err(Error::BadState);
                };
                if current.interrupt.pending() != sequence {
                    return Err(Error::Busy);
                }
                crate::vm::io::set_line(binding, route.irq, asserted);
                current
                    .interrupt
                    .complete(sequence, asserted)
                    .map_err(|_| Error::Busy)?;
                // Acknowledging observation is distinct from hardware rearm.
                // A still-asserted level keeps its token but is no longer a
                // continuously-ready userspace event.
                self.set_readable(current.interrupt.readable());
                Ok(())
            });
            binding.publish_changed_interrupts();
            result
        })
        .map_err(|_| Error::BadState)??;
        if !asserted {
            self.registrations.with(|registrations| {
                let registration = registrations[0].as_ref().ok_or(Error::BadState)?;
                // IRQ dispatch applies its mask before releasing the registry
                // lock. Rearm takes that same lock, then rechecks the token:
                // neither a late mask nor a newer pending IRQ can be lost.
                interrupt::enable_registered_shared_if(registration, || {
                    self.state
                        .with(|state| matches!(state, State::Active(route) if route.interrupt.can_rearm()))
                })
                .map_err(|_| Error::Interrupt)
            })?;
        }
        Ok(())
    }
}

impl private::Sealed for PhysicalDevice {}
impl private::UserExportable for PhysicalDevice {}
impl KernelObject for PhysicalDevice {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        self.diagnostic(cursor)
    }

    const KIND: ObjectKind = ObjectKind::PHYSICAL_DEVICE;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::DUPLICATE)
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::ASSIGN_DEVICE)
        .union(Rights::ACK_INTERRUPT)
        .union(Rights::WAIT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.events.signals,
            SignalMask::from_trusted_bits(
                hyper::abi::native::HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_READABLE
                    | hyper::abi::native::HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_DMA_FAULT,
            ),
        ))
    }
}
