// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Platform-bus binding and lifecycle for unclaimed NS16550 UARTs.
//!
//! This layer interprets FDT binding properties and acquires mapping
//! capabilities from `DriverServices`. It deliberately does not register an
//! interrupt handler: the kernel's selected host-console service owns that
//! policy. Generic UART instances therefore remain quiesced for their complete
//! platform-driver lifetime.

use alloc::boxed::Box;

use {
    crate::platform::DriverInstance, crate::platform::DriverServices,
    crate::platform::PlatformDevice, crate::platform::PlatformDriver, crate::platform::ProbeError,
};

use super::{InterruptMask, MmioAccess, Ns16550};

struct PlatformInstance {
    uart: Ns16550,
}

impl PlatformInstance {
    fn quiesce(&self) {
        self.uart.set_interrupt_mask(InterruptMask::NONE);
        self.uart.set_irq_output(false);
    }
}

impl DriverInstance for PlatformInstance {
    fn suspend(&mut self) -> Result<(), ProbeError> {
        self.quiesce();
        Ok(())
    }

    fn resume(&mut self) -> Result<(), ProbeError> {
        // This generic binding owns no IRQ registration or input consumer, so
        // resuming it means retaining the same quiesced state established by
        // probe rather than resurrecting firmware interrupt configuration.
        self.quiesce();
        Ok(())
    }

    fn remove(&mut self) -> Result<(), ProbeError> {
        self.quiesce();
        Ok(())
    }
}

pub struct Ns16550PlatformDriver;

impl PlatformDriver for Ns16550PlatformDriver {
    fn name(&self) -> &'static str {
        "ns16550"
    }

    fn compatible_table(&self) -> &'static [&'static str] {
        &["ns16550a", "ns16550", "uart8250", "snps,dw-apb-uart"]
    }

    fn probe(
        &self,
        device: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError> {
        let registers = device.registers().first().ok_or(ProbeError::Resource)?;
        let register_shift = match optional_u32(device, "reg-shift")? {
            Some(value) => u8::try_from(value).map_err(|_| ProbeError::Unsupported)?,
            None => 0,
        };
        let width = optional_u32(device, "reg-io-width")?.unwrap_or(1);
        let access = match width {
            1 => MmioAccess::Byte { register_shift },
            4 => MmioAccess::Word { register_shift },
            _ => return Err(ProbeError::Unsupported),
        };
        let mapping = services
            .map_mmio(*registers)
            .map_err(|_| ProbeError::Resource)?;
        let uart = Ns16550::from_mapped_mmio(mapping, access).map_err(|_| ProbeError::Resource)?;
        let instance = PlatformInstance { uart };
        instance.quiesce();
        let instance: Box<dyn DriverInstance> =
            hyper::mm::try_box(instance).map_err(|_| ProbeError::Resource)?;
        Ok(instance)
    }
}

fn optional_u32(device: &PlatformDevice, name: &str) -> Result<Option<u32>, ProbeError> {
    let Some(value) = device.property(name) else {
        return Ok(None);
    };
    let bytes: [u8; 4] = value.try_into().map_err(|_| ProbeError::Unsupported)?;
    Ok(Some(u32::from_be_bytes(bytes)))
}

pub static PLATFORM_DRIVER: Ns16550PlatformDriver = Ns16550PlatformDriver;
