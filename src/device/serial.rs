// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime ownership of the physical UART selected as the early console.

use hyper::hal::interrupt::{InterruptId, InterruptPriority, InterruptTrigger};
use hyper::hw::pl011 as reg;
use hyper::sync::InterruptSpinLock;
use hyper_drivers::platform::PlatformDevice;
use {
    hyper_drivers::serial::MmioAccess, hyper_drivers::serial::Ns16550,
    hyper_drivers::serial::Ns16550Error, hyper_drivers::serial::Ns16550FifoTrigger,
    hyper_drivers::serial::Pl011, hyper_drivers::serial::ReceivedByte,
};
use {
    hyper_platform::ConsoleInfo, hyper_platform::ConsoleKind,
    hyper_platform::ConsoleRegisterAccess, hyper_platform::PlatformInterruptTrigger,
};

use {
    crate::irq::interrupt, crate::irq::interrupt::HandlerResult,
    crate::irq::interrupt::Registration, crate::irq::interrupt::VirtualInterrupt,
};

type ConsoleLock = InterruptSpinLock<Option<RuntimeConsole>, crate::hal::irq::LocalMask>;
const MAX_DRAIN: usize = 32;

static HOST_CONSOLE: ConsoleLock = InterruptSpinLock::new(None);

struct RuntimeConsole {
    port: RuntimePort,
    // The console owns this capability for its entire installed lifetime.
    _interrupt_registration: Option<Registration>,
}

#[derive(Clone, Copy)]
enum RuntimePort {
    Pl011(Pl011),
    Ns16550(Ns16550),
}

#[derive(Clone, Copy)]
struct RuntimeReceivedByte {
    byte: u8,
    error: bool,
}

impl RuntimeReceivedByte {
    const EMPTY: Self = Self {
        byte: 0,
        error: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Capabilities {
    pub(super) driver: &'static str,
    pub(super) hardware_interrupt: u32,
    pub(super) virtual_interrupt: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    AlreadyInitialized,
    Interrupt(interrupt::Error),
    InvalidInterrupt,
    MissingDevice,
    Serial(Ns16550Error),
}

impl From<interrupt::Error> for Error {
    fn from(error: interrupt::Error) -> Self {
        Self::Interrupt(error)
    }
}

impl From<Ns16550Error> for Error {
    fn from(error: Ns16550Error) -> Self {
        Self::Serial(error)
    }
}

pub(super) fn initialize(
    boot: &crate::boot::Initialization,
    devices: &[PlatformDevice],
) -> Result<Option<Capabilities>, Error> {
    let Some(console) = boot.early_console() else {
        return Ok(None);
    };
    let device = discover(devices, console)?;
    let platform_interrupt = crate::hal::irq::decode_platform(device.interrupt_cells())
        .map_err(|_| Error::InvalidInterrupt)?;
    let trigger = match platform_interrupt.trigger {
        PlatformInterruptTrigger::Level => InterruptTrigger::Level,
        PlatformInterruptTrigger::Edge => InterruptTrigger::Edge,
    };
    let mapping = boot.early_console_mapping().ok_or(Error::MissingDevice)?;
    let port = bind_runtime_port(console, mapping)?;
    install(port, platform_interrupt.interrupt, trigger, boot)
}

fn discover(devices: &[PlatformDevice], console: ConsoleInfo) -> Result<&PlatformDevice, Error> {
    devices
        .iter()
        .find(|device| {
            let compatible = match console.kind {
                ConsoleKind::Pl011 => device.is_compatible("arm,pl011"),
                ConsoleKind::Ns16550 => ["ns16550a", "ns16550", "uart8250", "snps,dw-apb-uart"]
                    .iter()
                    .any(|compatible| device.is_compatible(compatible)),
            };
            compatible
                && device.registers().first().map(|range| range.start()) == Some(console.base)
        })
        .ok_or(Error::MissingDevice)
}

fn bind_runtime_port(
    console: ConsoleInfo,
    mapping: hyper_drivers::platform::PermanentMmioMapping,
) -> Result<RuntimePort, Error> {
    match console.kind {
        ConsoleKind::Pl011 => Pl011::from_mapped_mmio(mapping)
            .map(RuntimePort::Pl011)
            .map_err(|_| Error::MissingDevice),
        ConsoleKind::Ns16550 => {
            let access = match console.access {
                ConsoleRegisterAccess::Mmio8 { register_shift } => {
                    MmioAccess::Byte { register_shift }
                }
                ConsoleRegisterAccess::Mmio32 { register_shift } => {
                    MmioAccess::Word { register_shift }
                }
                ConsoleRegisterAccess::Native => MmioAccess::BYTE,
                ConsoleRegisterAccess::Port => {
                    return Err(Error::Serial(Ns16550Error::InvalidAccess));
                }
            };
            Ok(RuntimePort::Ns16550(Ns16550::from_mapped_mmio(
                mapping, access,
            )?))
        }
    }
}

fn install(
    port: RuntimePort,
    hardware_interrupt: u32,
    trigger: InterruptTrigger,
    boot: &crate::boot::Initialization,
) -> Result<Option<Capabilities>, Error> {
    if HOST_CONSOLE.with(|slot| slot.is_some()) {
        return Err(Error::AlreadyInitialized);
    }
    port.disable_interrupts();
    let interrupt_id = InterruptId::new(hardware_interrupt);
    let prepared = boot.interrupts().root_domain.prepare_shared_mapping(
        interrupt_id,
        InterruptPriority::Normal,
        trigger,
        0,
        handle_interrupt,
    )?;
    let virtual_interrupt = prepared.interrupt();
    HOST_CONSOLE.with(|slot| {
        *slot = Some(RuntimeConsole {
            port,
            _interrupt_registration: None,
        });
    });
    port.enable_runtime_input();
    let registration = match crate::irq::interrupt::activate(prepared) {
        Ok(registration) => registration,
        Err(failure) => {
            let (error, prepared) = failure.into_parts();
            port.disable_interrupts();
            HOST_CONSOLE.with(|slot| *slot = None);
            if let Err(discard) = crate::irq::interrupt::discard_prepared(prepared) {
                let (discard_error, _prepared) = discard.into_parts();
                crate::diagnostics::crash::fatal(format_args!(
                    "HypeR: serial IRQ activation rollback failed: {discard_error:?}"
                ));
            }
            return Err(Error::Interrupt(error));
        }
    };
    HOST_CONSOLE.with(|slot| {
        let Some(runtime) = slot.as_mut() else {
            registration.retain_permanently();
            crate::diagnostics::crash::fatal(format_args!(
                "HypeR: published runtime console disappeared before IRQ activation"
            ));
        };
        runtime._interrupt_registration = Some(registration);
    });
    Ok(Some(Capabilities {
        driver: port.name(),
        hardware_interrupt,
        virtual_interrupt: virtual_interrupt.get(),
    }))
}

fn handle_interrupt(
    _interrupt: VirtualInterrupt,
    _context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    let mut received = [RuntimeReceivedByte::EMPTY; MAX_DRAIN];
    let (count, handled) = HOST_CONSOLE.with(|slot| {
        let Some(runtime) = slot.as_ref() else {
            return (0, false);
        };
        drain_interrupt(&runtime.port, &mut received)
    });
    if !handled {
        return HandlerResult::NotHandled;
    }
    for byte in &received[..count] {
        if byte.error {
            super::console::record_receive_error();
        } else {
            super::console::receive(byte.byte);
        }
    }
    HandlerResult::Handled
}

#[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
pub(super) fn runtime_input_available() -> bool {
    HOST_CONSOLE.with(|slot| slot.is_some())
}

impl RuntimePort {
    const fn name(self) -> &'static str {
        match self {
            Self::Pl011(_) => "PL011",
            Self::Ns16550(_) => "NS16550",
        }
    }

    fn disable_interrupts(self) {
        match self {
            Self::Pl011(port) => port.disable_interrupts(),
            Self::Ns16550(port) => {
                port.set_interrupt_mask(hyper_drivers::serial::Ns16550InterruptMask::NONE)
            }
        }
    }

    fn enable_runtime_input(self) {
        match self {
            Self::Pl011(port) => port.enable_runtime_input(),
            Self::Ns16550(port) => port.enable_runtime_input(Ns16550FifoTrigger::OneByte),
        }
    }

    fn drain_interrupt(self, received: &mut [RuntimeReceivedByte]) -> (usize, bool) {
        match self {
            Self::Pl011(port) => drain_pl011(port, received),
            Self::Ns16550(port) => drain_ns16550(port, received),
        }
    }
}

fn drain_interrupt(port: &RuntimePort, received: &mut [RuntimeReceivedByte]) -> (usize, bool) {
    port.drain_interrupt(received)
}

fn drain_pl011(port: Pl011, received: &mut [RuntimeReceivedByte]) -> (usize, bool) {
    let status = port.masked_interrupt_status();
    if status.raw() == 0 {
        return (0, false);
    }
    let count = drain_received(received, || {
        port.try_read()
            .map(|byte: ReceivedByte| RuntimeReceivedByte {
                byte: byte.byte,
                error: byte.has_error(),
            })
    });
    if status.contains(reg::INT_ERROR_MASK) {
        port.clear_receive_errors();
    }
    port.clear_interrupts(status.raw() & (reg::INT_RT | reg::INT_ERROR_MASK));
    (count, true)
}

fn drain_ns16550(port: Ns16550, received: &mut [RuntimeReceivedByte]) -> (usize, bool) {
    if port.interrupt_identification()
        == hyper_drivers::serial::Ns16550InterruptIdentification::None
    {
        return (0, false);
    }
    let count = drain_received(received, || {
        port.try_read().map(|byte| RuntimeReceivedByte {
            byte: byte.byte,
            error: byte.has_error(),
        })
    });
    (count, true)
}

fn drain_received(
    received: &mut [RuntimeReceivedByte],
    mut next: impl FnMut() -> Option<RuntimeReceivedByte>,
) -> usize {
    let mut count = 0;
    for slot in received {
        let Some(byte) = next() else { break };
        *slot = byte;
        count += 1;
    }
    count
}
