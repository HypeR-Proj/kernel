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

struct Observer {
    routes: [u32; hyper::cpu::MAX_CPUS],
    sample: fn(SampleContext),
}

static OBSERVER: PublishedOnce<Observer> = PublishedOnce::new();

#[derive(Debug)]
pub struct AlreadyInstalled;
/// Installs the bounded, allocation-free observer before enabling PMU delivery.
/// Routes are indexed by logical CPU; zero denotes an unassigned CPU. PMU
/// interrupts are PPIs or SPIs, so zero cannot name a valid PMU route.
pub fn install(
    routes: [u32; hyper::cpu::MAX_CPUS],
    sample: fn(SampleContext),
) -> Result<(), AlreadyInstalled> {
    OBSERVER
        .publish(Observer { routes, sample })
        .map_err(|_| AlreadyInstalled)
}

/// Ordinary IRQs only check the immutable local route. Capture the interrupted
/// registers and enter profiling policy only for a matching PMU interrupt,
/// including a late pending interrupt after recording has stopped.
#[inline]
pub(crate) fn observe(interrupt: InterruptId, context: impl FnOnce() -> SampleContext) {
    if let Some(observer) = OBSERVER.get()
        && let Some(cpu) = crate::hal::cpu::current_index()
        && observer.routes[cpu.get()] == interrupt.get()
    {
        (observer.sample)(context());
    }
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub use crate::arch::aarch64::pmu::{available, initialize, overflowed, reload, start, stop};
