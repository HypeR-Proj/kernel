// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architectural one-shot clock-event registration and interrupt handling.

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::{InterruptId, InterruptPriority, InterruptTrigger};
use {hyper_platform::PlatformInterruptTrigger, hyper_platform::TimerInfo};

use {
    crate::irq::interrupt::HandlerResult, crate::irq::interrupt::IrqDomainId,
    crate::irq::interrupt::VirtualInterrupt,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Interrupt(crate::irq::interrupt::Error),
    Description(crate::hal::time::DescriptionError),
    Timer(crate::hal::time::Error),
    Time(crate::time::Error),
    InvalidCpuIndex,
    InconsistentCounterFrequency,
}

impl From<crate::irq::interrupt::Error> for Error {
    fn from(error: crate::irq::interrupt::Error) -> Self {
        Self::Interrupt(error)
    }
}

impl From<crate::hal::time::Error> for Error {
    fn from(error: crate::hal::time::Error) -> Self {
        Self::Timer(error)
    }
}

impl From<crate::hal::time::DescriptionError> for Error {
    fn from(error: crate::hal::time::DescriptionError) -> Self {
        Self::Description(error)
    }
}

impl From<crate::time::Error> for Error {
    fn from(error: crate::time::Error) -> Self {
        Self::Time(error)
    }
}

pub(super) fn initialize(
    info: TimerInfo,
    domain: IrqDomainId,
) -> Result<super::Capabilities, Error> {
    let description = crate::hal::time::describe(info)?;
    let hardware_interrupt = InterruptId::new(description.hardware.interrupt);
    let trigger = match description.hardware.trigger {
        PlatformInterruptTrigger::Level => InterruptTrigger::Level,
        PlatformInterruptTrigger::Edge => InterruptTrigger::Edge,
    };
    let (virtual_interrupt, registration) = domain.register_shared_mapping(
        hardware_interrupt,
        InterruptPriority::Normal,
        trigger,
        0,
        handle_host_timer,
    )?;
    let counter_frequency_hz = match crate::time::counter_frequency_hz() {
        Ok(frequency) => frequency,
        Err(error) => {
            rollback_registered_mapping(registration, virtual_interrupt);
            return Err(error.into());
        }
    };
    if let Err(error) = start_local_clock_event(counter_frequency_hz) {
        rollback_registered_mapping(registration, virtual_interrupt);
        return Err(error);
    }

    // The architectural timer is part of the kernel runtime and has no
    // teardown phase. Publish its permanent ownership only after every
    // fallible initialization step has completed.
    registration.retain_permanently();
    Ok(super::Capabilities {
        counter_frequency_hz,
        hardware_interrupt,
        virtual_interrupt,
        guest_timer: super::GuestTimerSource {
            interrupt: description.guest_virtual_interrupt,
            requires_host_mapping: description.map_guest_virtual_interrupt,
        },
    })
}

fn rollback_registered_mapping(
    registration: crate::irq::interrupt::Registration,
    interrupt: VirtualInterrupt,
) {
    match crate::irq::interrupt::unregister(registration) {
        Ok(()) => rollback_unused_mapping(interrupt),
        Err(failure) => retain_failed_registration(failure),
    }
}

fn rollback_unused_mapping(interrupt: VirtualInterrupt) {
    if let Err(error) = crate::irq::interrupt::unmap(interrupt) {
        crate::pr_warn!("HypeR: IRQ mapping rollback failed: {error:?}");
    }
}

fn retain_failed_registration(failure: crate::irq::interrupt::UnregisterFailure) {
    let (error, registration) = failure.into_parts();
    crate::pr_warn!("HypeR: retaining IRQ handler after rollback failed: {error:?}");
    registration.retain_permanently();
}

/// Starts the already-mapped architectural PPI on a secondary CPU.
pub(super) fn initialize_local_cpu() -> Result<(), Error> {
    current_cpu()?;
    start_local_clock_event(crate::time::counter_frequency_hz()?)
}

fn handle_host_timer(
    _interrupt: VirtualInterrupt,
    _context: usize,
    _frame: &crate::hal::irq::InterruptFrame<'_>,
) -> HandlerResult {
    if let Err(error) = crate::time::handle_timer_interrupt() {
        crate::irq::exception::fatal_timer(error);
    }
    HandlerResult::Handled
}

fn start_local_clock_event(counter_frequency_hz: u64) -> Result<(), Error> {
    if crate::hal::time::counter_frequency_hz()? != counter_frequency_hz {
        return Err(Error::InconsistentCounterFrequency);
    }
    crate::time::initialize_local_timer_queue()?;
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    validation::arm(1)?;
    Ok(())
}

fn current_cpu() -> Result<CpuIndex, Error> {
    crate::cpu::current_index().ok_or(Error::InvalidCpuIndex)
}

/// Three independent comparator expiries prove local interrupt delivery and
/// rearming. Ordinary images have no probe; test images stop after this finite
/// sequence, so timer validation cannot conceal a missing scheduler deadline.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) mod validation {
    use hyper::cpu::PerCpu;
    use hyper::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const PROBE_DELAY_NS: u64 = 1_000_000;
    const PROBE_COUNT: u64 = 3;
    static COMPLETE: PerCpu<AtomicBool> =
        PerCpu::new([const { AtomicBool::new(false) }; hyper::cpu::MAX_CPUS]);
    static EXPECTED_CPUS: AtomicUsize = AtomicUsize::new(0);
    static REPORTED: AtomicBool = AtomicBool::new(false);

    pub(super) fn arm(generation: u64) -> Result<(), crate::time::Error> {
        let deadline = crate::time::deadline_after(PROBE_DELAY_NS)?;
        super::super::timers::publish_health_probe(generation, deadline)
    }

    pub(in crate::time) fn observe(generation: u64) -> Result<(), crate::time::Error> {
        if generation < PROBE_COUNT {
            return arm(generation + 1);
        }
        let cpu = crate::cpu::current_index().ok_or(crate::time::Error::InvalidCpuIndex)?;
        COMPLETE[cpu].store(true, Ordering::SeqCst);
        report();
        Ok(())
    }

    pub(in crate::time) fn expect_cpus(count: usize) {
        // Together with COMPLETE's SeqCst operations, the admission thread
        // and the last probe cannot both miss the other's publication.
        EXPECTED_CPUS.store(count, Ordering::SeqCst);
        report();
    }

    fn report() {
        let count = EXPECTED_CPUS.load(Ordering::SeqCst);
        if count == 0
            || !COMPLETE
                .iter()
                .take(count)
                .all(|done| done.load(Ordering::SeqCst))
            || REPORTED.swap(true, Ordering::AcqRel)
        {
            return;
        }
        crate::pr_info!("HypeR: one-shot timer IRQs validated on {count} CPUs");
    }
}
