// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected host counter and timer capabilities.
//!
//! Kernel time policy consumes this facade; the architecture backend owns the
//! counter instructions, comparator programming, and platform timer decoding.
//! Guest virtual-timer state and injection remain VM capabilities.

use hyper::hal::timer::{DeadlineTimer, MonotonicCounter};

/// Failure reported by the selected counter or local comparator backend.
///
/// The backend value remains private so kernel policy cannot depend on
/// architecture-specific error variants while diagnostics retain its detail.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Error(crate::arch::time::Error);

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error {
    const fn from_backend(error: crate::arch::time::Error) -> Self {
        Self(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptionError {
    InvalidInterruptTrigger,
    UnsupportedTimer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Description {
    pub hardware: hyper::platform::PlatformInterrupt,
    pub guest_virtual_interrupt: hyper::hal::interrupt::InterruptId,
    pub map_guest_virtual_interrupt: bool,
}

pub fn prepare(platform: &super::platform::EssentialInfo) -> Result<(), Error> {
    crate::arch::time::prepare(platform.as_backend()).map_err(Error::from_backend)
}

pub fn describe(info: hyper::platform::TimerInfo) -> Result<Description, DescriptionError> {
    let description = crate::arch::time::describe(info).map_err(|error| match error {
        #[cfg(CONFIG_ARCH_AARCH64)]
        crate::arch::time::DescriptionError::InvalidInterruptTrigger => {
            DescriptionError::InvalidInterruptTrigger
        }
        crate::arch::time::DescriptionError::UnsupportedTimer => DescriptionError::UnsupportedTimer,
    })?;
    Ok(Description {
        hardware: description.hardware,
        guest_virtual_interrupt: description.guest_virtual_interrupt,
        map_guest_virtual_interrupt: description.map_guest_virtual_interrupt,
    })
}

pub fn counter_frequency_hz() -> Result<u64, Error> {
    crate::arch::time::Counter::frequency_hz().map_err(Error::from_backend)
}

pub fn read_counter() -> u64 {
    crate::arch::time::Counter::read()
}

pub fn boot_counter() -> u64 {
    crate::arch::time::boot_counter()
}

pub fn program_deadline(deadline: u64) -> Result<(), Error> {
    crate::arch::time::Timer::set_deadline(deadline).map_err(Error::from_backend)
}

pub fn mask_local_timer() {
    crate::arch::time::Timer::mask();
}

pub fn disable_local_timer() {
    crate::arch::time::Timer::disable();
}
