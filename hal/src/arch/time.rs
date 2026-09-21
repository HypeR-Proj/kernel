// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture monotonic time mechanisms.
//!
//! Kernel timekeeping owns clock and timer policy. This facade selects the
//! machine counter, one-shot comparator, and platform timer description
//! without exposing an architecture backend to policy code.

use hyper::sync::atomic::{AtomicU64, Ordering};

static BOOT_COUNTER_TICKS: AtomicU64 = AtomicU64::new(0);

pub use super::imp::{
    ArchitectureCounter as Counter, ArchitectureTimer as Timer, TimerError as Error,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptionError {
    #[cfg(CONFIG_ARCH_AARCH64)]
    InvalidInterruptTrigger,
    UnsupportedTimer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Description {
    pub hardware: hyper::platform::PlatformInterrupt,
    pub guest_virtual_interrupt: hyper::hal::interrupt::InterruptId,
    pub map_guest_virtual_interrupt: bool,
}

pub(crate) use super::imp::decode_kernel_timer as describe;

pub(crate) fn record_boot_counter(ticks: u64) {
    BOOT_COUNTER_TICKS.store(ticks, Ordering::Relaxed);
}

pub(crate) fn boot_counter() -> u64 {
    BOOT_COUNTER_TICKS.load(Ordering::Relaxed)
}

pub(crate) fn prepare(platform: &super::platform::EssentialInfo) -> Result<(), Error> {
    super::imp::prepare_timekeeping(platform.as_backend())
}
