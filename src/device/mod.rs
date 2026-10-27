// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel physical-device initialization policy.
//!
//! Boot-critical firmware services are activated before interrupt setup;
//! discoverable platform devices bind later through the driver framework.

pub(crate) mod assigned;
pub(crate) mod console;
pub mod cpu_power;
pub(crate) mod dma;
mod firmware;
pub(crate) mod iommu;
mod platform_bus;
mod serial;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    CpuPower(cpu_power::Error),
    DriverFramework(platform_bus::Error),
    MissingCpuPower,
}

/// Initializes boot-critical firmware interfaces before host interrupts.
pub(crate) fn early_initialize(
    boot: &crate::boot::Initialization,
) -> Result<(), InitializationError> {
    let info = boot
        .essential()
        .cpu_power()
        .ok_or(InitializationError::MissingCpuPower)?;
    let capabilities = cpu_power::initialize(info).map_err(InitializationError::CpuPower)?;
    crate::pr_info!(
        "HypeR: CPU power interface version {}.{}: on={}, off={}, suspend={}, reset={}",
        capabilities.version.major,
        capabilities.version.minor,
        capabilities.cpu_on,
        capabilities.cpu_off,
        capabilities.cpu_suspend,
        capabilities.system_reset
    );
    Ok(())
}

/// Enumerates platform devices, promotes earlycon, and binds built-in drivers.
pub(crate) fn platform_device_initialize(
    boot: &crate::boot::Initialization,
) -> Result<(), InitializationError> {
    let report = platform_bus::initialize(boot).map_err(InitializationError::DriverFramework)?;
    match report.console {
        Ok(Some(capabilities)) => crate::pr_info!(
            "HypeR: {} runtime input active: INTID {}, VIRQ {}",
            capabilities.driver,
            capabilities.hardware_interrupt,
            capabilities.virtual_interrupt
        ),
        Ok(None) => crate::pr_warn!(
            "HypeR: selected early console has no runtime input driver; console input disabled"
        ),
        Err(error) => crate::pr_warn!(
            "HypeR: early console remains output-only; runtime input unavailable: {error:?}"
        ),
    }
    crate::pr_info!(
        "HypeR: platform bus: {} bound, {} unmatched, {} deferred, {} failed",
        report.drivers.bound,
        report.drivers.unmatched,
        report.drivers.deferred,
        report.drivers.failed
    );
    Ok(())
}
