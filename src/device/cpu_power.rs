// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel-facing CPU power service.

use hyper::hal::cpu_power::{
    CpuAffinityState, CpuHardwareId, CpuPower, CpuPowerCapabilities, ResumeAddress, SuspendState,
};
use hyper::sync::InterruptSpinLock;
use hyper_platform::CpuPowerInfo;

type CpuPowerLock =
    InterruptSpinLock<Option<crate::hal::cpu::PowerController>, crate::hal::irq::LocalMask>;

static CPU_POWER: CpuPowerLock = InterruptSpinLock::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyInitialized,
    NotInitialized,
    Architecture(crate::hal::cpu::PowerError),
}

impl From<crate::hal::cpu::PowerError> for Error {
    fn from(error: crate::hal::cpu::PowerError) -> Self {
        Self::Architecture(error)
    }
}

pub fn initialize(info: CpuPowerInfo) -> Result<CpuPowerCapabilities, Error> {
    CPU_POWER.with(|slot| {
        if slot.is_some() {
            return Err(Error::AlreadyInitialized);
        }
        let controller = crate::hal::cpu::initialize_power(info)?;
        let capabilities = controller.capabilities();
        *slot = Some(controller);
        Ok(capabilities)
    })
}

pub fn capabilities() -> Result<CpuPowerCapabilities, Error> {
    Ok(controller()?.capabilities())
}

/// Starts a CPU through the installed architecture power service.
///
/// # Safety
///
/// `entry` and `context` must describe a valid, published resume trampoline
/// invocation and remain live until the target CPU consumes the context.
pub unsafe fn cpu_on(
    target: CpuHardwareId,
    entry: ResumeAddress,
    context: u64,
) -> Result<(), Error> {
    // SAFETY: This wrapper forwards the caller's valid resume entry, live
    // coherent context, and target-CPU exclusivity contract unchanged.
    unsafe { controller()?.cpu_on(target, entry, context) }.map_err(Error::from)
}

pub fn cpu_off() -> Result<(), Error> {
    controller()?.cpu_off().map_err(Error::from)
}

/// Suspends this CPU and optionally resumes through a supplied trampoline.
///
/// # Safety
///
/// For powerdown states, `entry` and `context` must remain valid and coherent
/// until firmware resumes the CPU and the trampoline consumes the context.
pub unsafe fn cpu_suspend(
    state: SuspendState,
    entry: ResumeAddress,
    context: u64,
) -> Result<(), Error> {
    // SAFETY: For powerdown states the caller supplies the valid pinned resume
    // context required by the architecture power controller.
    unsafe { controller()?.cpu_suspend(state, entry, context) }.map_err(Error::from)
}

pub fn affinity_info(
    target: CpuHardwareId,
    lowest_affinity_level: u8,
) -> Result<CpuAffinityState, Error> {
    controller()?
        .affinity_info(target, lowest_affinity_level)
        .map_err(Error::from)
}

pub fn system_off() -> Result<(), Error> {
    controller()?.system_off().map_err(Error::from)
}

pub fn system_reset() -> Result<(), Error> {
    controller()?.system_reset().map_err(Error::from)
}

fn controller() -> Result<crate::hal::cpu::PowerController, Error> {
    CPU_POWER.with(|slot| {
        let controller = *slot.as_ref().ok_or(Error::NotInitialized)?;
        Ok(controller)
    })
}
