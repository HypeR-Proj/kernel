// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Host-owned performance monitoring. No guest or user stack is dereferenced here.

use hyper::hal::interrupt::InterruptId;
use hyper::sync::PublishedOnce;

/// Register snapshot borrowed from one masked physical IRQ entry.
#[derive(Clone, Copy)]
pub struct SampleContext {
    pub pc: usize,
    pub sp: usize,
    pub fp: usize,
    /// 0: host kernel, 1: Native user, 2: guest.
    pub world: u64,
}

static OBSERVER: PublishedOnce<fn(InterruptId, SampleContext)> = PublishedOnce::new();

#[derive(Debug)]
pub struct AlreadyInstalled;
/// Installs the bounded, allocation-free observer before enabling PMU delivery.
pub fn install(observer: fn(InterruptId, SampleContext)) -> Result<(), AlreadyInstalled> {
    OBSERVER.publish(observer).map_err(|_| AlreadyInstalled)
}

pub(crate) fn observe(interrupt: InterruptId, context: SampleContext) {
    if let Some(observer) = OBSERVER.get() {
        observer(interrupt, context);
    }
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub use crate::arch::aarch64::pmu::{available, initialize, overflowed, reload, start, stop};
