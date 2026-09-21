// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::timer::{DeadlineTimer, MonotonicCounter};
use hyper::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const IA32_TSC_DEADLINE: u32 = 0x6e0;
static FREQUENCY: AtomicU64 = AtomicU64::new(0);
static APIC_FREQUENCY: AtomicU64 = AtomicU64::new(0);
static USE_TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
static DEADLINES: [AtomicU64; hyper::config::MAX_CPUS as usize] =
    [const { AtomicU64::new(0) }; hyper::config::MAX_CPUS as usize];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidFrequency,
    InvalidCpuIndex,
}

pub struct TscCounter;
pub struct TscDeadlineTimer;

pub fn set_frequency(frequency: u64) -> Result<(), Error> {
    if frequency == 0 {
        return Err(Error::InvalidFrequency);
    }
    match FREQUENCY.compare_exchange(0, frequency, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => initialize_backend(frequency),
        Err(current) if current == frequency => Ok(()),
        Err(_) => Err(Error::InvalidFrequency),
    }
}

fn initialize_backend(tsc_frequency: u64) -> Result<(), Error> {
    let features = core::arch::x86_64::__cpuid(1).ecx;
    if features & (1 << 24) != 0 {
        USE_TSC_DEADLINE.store(true, Ordering::Release);
        return Ok(());
    }
    super::interrupt_controller::begin_timer_calibration();
    let start = TscCounter::read();
    let interval = (tsc_frequency / 1_000).max(1);
    while TscCounter::read().wrapping_sub(start) < interval {
        core::hint::spin_loop();
    }
    let elapsed = u64::from(super::interrupt_controller::end_timer_calibration());
    let apic_frequency = elapsed.checked_mul(1_000).ok_or(Error::InvalidFrequency)?;
    if apic_frequency == 0 {
        return Err(Error::InvalidFrequency);
    }
    APIC_FREQUENCY.store(apic_frequency, Ordering::Release);
    Ok(())
}

pub fn prepare_interrupt_enable() {
    let Some(deadline) = DEADLINES.get(super::current_cpu_index()) else {
        return;
    };
    let deadline = deadline.load(Ordering::Acquire);
    if deadline == 0 || hardware_timer_armed() {
        return;
    }
    let now = TscCounter::read();
    let armed = if (deadline.wrapping_sub(now) as i64) <= 0 {
        now.wrapping_add(minimum_rearm_interval())
    } else {
        deadline
    };
    arm_deadline(armed, now);
}

impl MonotonicCounter for TscCounter {
    type Error = Error;

    fn frequency_hz() -> Result<u64, Self::Error> {
        match FREQUENCY.load(Ordering::Acquire) {
            0 => Err(Error::InvalidFrequency),
            frequency => Ok(frequency),
        }
    }

    fn read() -> u64 {
        let low: u32;
        let high: u32;
        // SAFETY: RDTSC has no pointer operands and is supported by the target profile.
        unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack)) };
        (u64::from(high) << 32) | u64::from(low)
    }
}

impl DeadlineTimer for TscDeadlineTimer {
    type Error = Error;

    fn set_deadline(deadline: u64) -> Result<(), Self::Error> {
        let slot = DEADLINES
            .get(super::current_cpu_index())
            .ok_or(Error::InvalidCpuIndex)?;
        let now = TscCounter::read();
        let deadline = if (deadline.wrapping_sub(now) as i64) <= 0 {
            now.wrapping_add(minimum_rearm_interval())
        } else {
            deadline
        };
        slot.store(deadline, Ordering::Release);
        arm_deadline(deadline, now);
        Ok(())
    }

    fn mask() {
        super::interrupt_controller::mask_timer();
    }

    fn disable() {
        if let Some(slot) = DEADLINES.get(super::current_cpu_index()) {
            slot.store(0, Ordering::Release);
        }
        write_msr(IA32_TSC_DEADLINE, 0);
        super::interrupt_controller::program_oneshot_timer(0);
        Self::mask();
    }
}

fn minimum_rearm_interval() -> u64 {
    (FREQUENCY.load(Ordering::Acquire) / 1_000).max(1)
}

fn hardware_timer_armed() -> bool {
    if USE_TSC_DEADLINE.load(Ordering::Acquire) {
        read_msr(IA32_TSC_DEADLINE) != 0
    } else {
        super::interrupt_controller::timer_current_count() != 0
    }
}

fn arm_deadline(deadline: u64, now: u64) {
    if USE_TSC_DEADLINE.load(Ordering::Acquire) {
        super::interrupt_controller::unmask_timer();
        write_msr(IA32_TSC_DEADLINE, deadline);
        return;
    }
    let tsc_frequency = FREQUENCY.load(Ordering::Acquire);
    let apic_frequency = APIC_FREQUENCY.load(Ordering::Acquire);
    let delta = deadline.wrapping_sub(now).max(1);
    let count = (u128::from(delta) * u128::from(apic_frequency) / u128::from(tsc_frequency))
        .clamp(1, u128::from(u32::MAX)) as u32;
    super::interrupt_controller::program_oneshot_timer(count);
}

fn write_msr(msr: u32, value: u64) {
    // SAFETY: Callers use timer MSRs validated during timer initialization.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        )
    };
}

fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: Callers use timer MSRs validated during timer initialization.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        )
    };
    (u64::from(high) << 32) | u64::from(low)
}
