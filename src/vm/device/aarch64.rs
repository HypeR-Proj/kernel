// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` SBSA UART and virtual GIC guest-platform device service.

use alloc::boxed::Box;
use hyper::mm::try_box;
use hyper::sync::InterruptSpinLock;
use hyper_vm_models::arm::gic::GicInterruptId;
use {
    hyper_vm_models::aarch64::device::sbsa_uart::AccessError,
    hyper_vm_models::aarch64::device::sbsa_uart::REFERENCE_BASE,
    hyper_vm_models::aarch64::device::sbsa_uart::REFERENCE_INTERRUPT,
    hyper_vm_models::aarch64::device::sbsa_uart::REFERENCE_SIZE,
    hyper_vm_models::aarch64::device::sbsa_uart::SbsaUart,
};
use {
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

use super::super::super::registry::{VmBinding, VmId};

use super::super::serial_timeout::SerialTimeout;
use crate::vm::serial::Route;

type ConsoleLock = InterruptSpinLock<Console, crate::hal::irq::LocalMask>;
struct Console {
    uart: SbsaUart,
    closed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    InvalidInterrupt,
    Timer,
    Closed,
    Model(AccessError),
}

pub(crate) struct VirtualDeviceSet {
    // Allocate the FIFO state once before publication rather than copying it
    // through the by-value VM construction transaction on the kernel stack.
    console: Box<ConsoleLock>,
    console_interrupt: GicInterruptId,
    timeout: SerialTimeout,
    virtual_serial: Option<super::super::VirtualSerialBinding>,
}

struct MmioOutcome {
    value: Option<u64>,
}

impl VirtualDeviceSet {
    fn new(virtual_serial: Option<super::super::VirtualSerialBinding>) -> Result<Self, Error> {
        let console_interrupt =
            GicInterruptId::new(REFERENCE_INTERRUPT).ok_or(Error::InvalidInterrupt)?;
        let frequency = crate::time::counter_frequency_hz().map_err(|_| Error::Timer)?;
        Ok(Self {
            console: try_box(InterruptSpinLock::new(Console {
                uart: SbsaUart::with_clock(frequency),
                closed: false,
            }))
            .map_err(|_| Error::Allocation)?,
            timeout: SerialTimeout::try_new(uart_timeout).map_err(|_| Error::Timer)?,
            console_interrupt,
            virtual_serial,
        })
    }

    fn access(
        &self,
        access: MmioAccess,
        update: impl FnOnce(GicInterruptId, bool) -> Result<(), Error>,
    ) -> Result<Option<MmioOutcome>, Error> {
        let address = access.address().get();
        let Some(offset) = address.checked_sub(REFERENCE_BASE) else {
            return Ok(None);
        };
        if offset >= REFERENCE_SIZE {
            return Ok(None);
        }
        let value = self.console.with(|console| {
            if console.closed {
                return Err(Error::Closed);
            }
            let now = crate::time::monotonic_ticks();
            let uart = &mut console.uart;
            pump_serial(uart, self.virtual_serial.as_ref(), now);
            let value = match access.operation() {
                MmioOperation::Read => {
                    Some(uart.read(offset, access.size()).map_err(Error::Model)?)
                }
                MmioOperation::Write(value) => {
                    match uart.write(offset, access.size(), value, now) {
                        Ok(()) => {}
                        // As with hardware, software must observe TXFF before
                        // writing. A write to a full FIFO is ignored, never an
                        // overwrite of an earlier character or a VM failure.
                        Err(AccessError::TransmitFull) => {}
                        Err(error) => return Err(Error::Model(error)),
                    }
                    None
                }
            };
            pump_serial(uart, self.virtual_serial.as_ref(), now);
            // One UART lock orders FIFO state, output publication, and the
            // guest IRQ level across all vCPUs. No host I/O runs here.
            update(self.console_interrupt, uart.interrupt_asserted())?;
            Ok(value)
        })?;
        self.reschedule()?;
        Ok(Some(MmioOutcome { value }))
    }

    pub(in crate::vm) fn bind_virtual_serial(&self, vm: VmId, vcpu: u32) {
        self.timeout.bind(Route { vm, vcpu });
        if let Some(output) = &self.virtual_serial {
            output.bind(crate::vm::serial::Route { vm, vcpu });
        }
    }

    pub(in crate::vm) fn disconnect_virtual_serial(&self, vm: VmId) {
        if let Some(output) = &self.virtual_serial {
            output.disconnect(vm);
        }
    }

    pub(in crate::vm) fn quiesce(&self) -> Result<(), Error> {
        self.timeout
            .quiesce(|| self.console.with(|console| console.closed = true))
            .map_err(|_| Error::Timer)
    }

    fn reschedule(&self) -> Result<(), Error> {
        self.timeout
            .reschedule(|| {
                self.console.with(|console| {
                    if console.closed {
                        None
                    } else {
                        console.uart.next_timeout()
                    }
                })
            })
            .map_err(|_| Error::Timer)
    }

    fn receive_from_virtual_serial(
        &self,
        timeout_only: bool,
        update: impl FnOnce(GicInterruptId, bool) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.console.with(|console| {
            if console.closed {
                return Ok(());
            }
            let now = crate::time::monotonic_ticks();
            if timeout_only {
                console.uart.advance(now);
            } else {
                pump_serial(&mut console.uart, self.virtual_serial.as_ref(), now);
            }
            update(self.console_interrupt, console.uart.interrupt_asserted())
        })?;
        if !timeout_only {
            self.reschedule()?;
        }
        Ok(())
    }
}

fn pump_serial(
    uart: &mut SbsaUart,
    binding: Option<&super::super::VirtualSerialBinding>,
    now: u64,
) {
    uart.advance(now);
    while let Some(byte) = uart.pending_transmit() {
        if binding.is_some_and(|binding| !binding.try_write_byte(byte)) {
            break;
        }
        let _ = uart.take_transmitted();
    }
    let Some(binding) = binding else {
        return;
    };
    while uart.can_receive() {
        let Some(byte) = binding.pop_guest_input() else {
            break;
        };
        let _ = uart.receive(byte, now);
    }
}

pub(super) const fn dynamic_allocation_bytes() -> usize {
    SerialTimeout::allocation_bytes() + core::mem::size_of::<ConsoleLock>()
}
pub(super) const fn timer_count() -> u64 {
    1
}

fn uart_timeout(route: Route) {
    receive_serial(route, true);
}

pub(super) fn prepare(
    virtual_serial: Option<super::super::VirtualSerialBinding>,
) -> Result<VirtualDeviceSet, Error> {
    VirtualDeviceSet::new(virtual_serial)
}

pub(super) fn supports_configuration(profile: u32, memory_base: u64, memory_size: u64) -> bool {
    profile == hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE as u32
        && memory_base
            == hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_GUEST_RAM_BASE
        && memory_size != 0
        && memory_base.checked_add(memory_size).is_some()
}

pub(super) const fn default_timer_interrupt() -> hyper_vm_models::interrupt::VirtualInterruptId {
    hyper_vm_models::interrupt::VirtualInterruptId::new(
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_TIMER_INTERRUPT as u32,
    )
}

pub(super) fn supports_userspace_mmio(profile: u32, base: u64, length: u64) -> bool {
    use hyper::abi::native::{
        HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE,
        HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_USER_MMIO_BASE as APERTURE_BASE,
        HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_USER_MMIO_SIZE as APERTURE_SIZE,
    };
    profile == HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE as u32
        && length != 0
        && base.is_multiple_of(hyper::mm::PAGE_SIZE)
        && length.is_multiple_of(hyper::mm::PAGE_SIZE)
        && base >= APERTURE_BASE
        && base
            .checked_add(length)
            .is_some_and(|end| end <= APERTURE_BASE + APERTURE_SIZE)
}

#[must_use]
pub(crate) struct MmioDispatch {
    action: MmioAction,
}

impl MmioDispatch {
    const fn new(action: MmioAction) -> Self {
        Self { action }
    }

    pub(crate) const fn into_action(self) -> MmioAction {
        self.action
    }
}

pub(super) fn dispatch_mmio(
    execution: &mut crate::vm::vcpu::VcpuExecution,
    access: MmioAccess,
) -> MmioDispatch {
    enum Resolution {
        Action(MmioAction),
        Unhandled(Option<super::super::super::UnhandledMmioReport>),
    }

    fn resolve_access(operation: MmioOperation, outcome: MmioOutcome) -> Resolution {
        match (operation, outcome.value) {
            (MmioOperation::Read, Some(value)) => {
                Resolution::Action(MmioAction::CompleteRead(value))
            }
            (MmioOperation::Write(_), _) => Resolution::Action(MmioAction::CompleteWrite),
            (MmioOperation::Read, None) => Resolution::Action(MmioAction::Stop),
        }
    }

    let Some((vcpu_id, resolution)) = (|| {
        let (binding, hardware, vcpu_id, interrupts) = execution.device_context()?;
        let resolution = match handle_mmio(binding, hardware, interrupts, vcpu_id, access) {
            Ok(Some(outcome)) => resolve_access(access.operation(), outcome),
            Ok(None) => match handle_gic(hardware, interrupts, vcpu_id, access) {
                Ok(Some(outcome)) => {
                    if crate::hal::vm::interrupt_access_pending(interrupts, vcpu_id) {
                        Resolution::Action(MmioAction::Deferred)
                    } else {
                        resolve_access(access.operation(), outcome)
                    }
                }
                Ok(None) => match binding
                    .route_physical_mmio(access)
                    .or_else(|| binding.route_io_mmio(access))
                    .or_else(|| binding.lifecycle().route_mmio(vcpu_id, access))
                {
                    Some(action) => Resolution::Action(action),
                    None => Resolution::Unhandled(binding.admit_unhandled_mmio(vcpu_id, access)),
                },
                Err(_) => Resolution::Action(MmioAction::Stop),
            },
            Err(_) => Resolution::Action(MmioAction::Stop),
        };
        binding.publish_changed_interrupts();
        Some((vcpu_id, resolution))
    })() else {
        return MmioDispatch::new(MmioAction::Stop);
    };
    let action = match resolution {
        Resolution::Action(action) => action,
        Resolution::Unhandled(report) => {
            publish_terminal_mmio_report(execution, vcpu_id, report);
            MmioAction::Unhandled
        }
    };
    MmioDispatch::new(action)
}

fn handle_mmio(
    binding: &VmBinding,
    hardware: &mut crate::hal::vm::VcpuHardwareState,
    interrupts: &super::super::super::VmInterruptController,
    vcpu_id: u32,
    access: MmioAccess,
) -> Result<Option<MmioOutcome>, Error> {
    binding.devices().access(access, |interrupt, asserted| {
        crate::hal::vm::update_guest_device_interrupt(
            hardware, vcpu_id, interrupts, interrupt, asserted,
        )
        .map_err(|_| Error::InvalidInterrupt)
    })
}

fn handle_gic(
    hardware: &mut crate::hal::vm::VcpuHardwareState,
    interrupts: &super::super::super::VmInterruptController,
    vcpu_id: u32,
    access: MmioAccess,
) -> Result<Option<MmioOutcome>, super::gic::Error> {
    let Some(decoded) = super::gic::decode(access, interrupts.vcpu_count())? else {
        return Ok(None);
    };
    let value = super::gic::access(hardware, interrupts, vcpu_id, decoded, access.operation())?;
    Ok(Some(MmioOutcome { value }))
}

fn publish_terminal_mmio_report(
    execution: &mut crate::vm::vcpu::VcpuExecution,
    vcpu_id: u32,
    report: Option<super::super::super::UnhandledMmioReport>,
) {
    if let Some(report) = report
        && execution.publish_terminal_mmio_report(report).is_err()
    {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: duplicate terminal MMIO report for active vCPU {vcpu_id}"
        ));
    }
}

pub(super) fn kick_virtual_serial(route: Route) {
    receive_serial(route, false);
}

fn receive_serial(route: Route, timeout_only: bool) {
    let delivery = super::super::super::registry::with_binding(route.vm, |binding| {
        binding
            .devices()
            .receive_from_virtual_serial(timeout_only, |interrupt, asserted| {
                crate::hal::vm::update_saved_guest_device_interrupt(
                    binding.interrupts(),
                    route.vcpu,
                    interrupt,
                    asserted,
                )
                .map_err(|_| Error::InvalidInterrupt)
            })
            .map_err(|_| ())?;
        binding.publish_changed_interrupts();
        Ok::<(), ()>(())
    });
    if !matches!(delivery, Ok(Ok(()))) {
        let _ = super::super::super::registry::with_binding(route.vm, |binding| {
            binding.devices().disconnect_virtual_serial(route.vm);
        });
    }
}
