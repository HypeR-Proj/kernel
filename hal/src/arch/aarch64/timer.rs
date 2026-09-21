// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::drivers::timer::arm_generic::{CONTROL_STATUS, VirtualTimerState};
use hyper::hal::timer::{DeadlineTimer, MonotonicCounter};

use super::registers;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidFrequency,
    InvalidCpuIndex,
}

/// The physical system counter shared by all processing elements.
pub struct ArmGenericCounter;

impl MonotonicCounter for ArmGenericCounter {
    type Error = Error;

    fn frequency_hz() -> Result<u64, Self::Error> {
        let frequency = counter_frequency();
        if frequency == 0 {
            Err(Error::InvalidFrequency)
        } else {
            Ok(frequency)
        }
    }

    fn read() -> u64 {
        physical_count()
    }
}

pub type VirtualTimerContext = VirtualTimerState;

/// EL2 physical timer backed by `CNTHP_EL2` system registers.
pub struct El2PhysicalTimer;

impl DeadlineTimer for El2PhysicalTimer {
    type Error = Error;

    fn set_deadline(deadline: u64) -> Result<(), Self::Error> {
        let _ = current_cpu()?;
        write_deadline(deadline);
        write_control(registers::CNT_CTL_ENABLE);
        Ok(())
    }

    fn mask() {
        write_control(registers::CNT_CTL_ENABLE | registers::CNT_CTL_IMASK);
    }

    fn disable() {
        write_control(0);
    }
}

fn current_cpu() -> Result<usize, Error> {
    let cpu = super::current_cpu_index();
    (cpu < hyper::config::MAX_CPUS as usize)
        .then_some(cpu)
        .ok_or(Error::InvalidCpuIndex)
}

fn counter_frequency() -> u64 {
    let frequency: u64;
    // SAFETY: CNTFRQ_EL0 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "mrs {frequency}, CNTFRQ_EL0",
            frequency = out(reg) frequency,
            options(nomem, nostack, preserves_flags)
        );
    }
    frequency
}

fn physical_count() -> u64 {
    let count: u64;
    // SAFETY: CNTPCT_EL0 is readable at EL2 and has no side effects.
    unsafe {
        asm!(
            "isb",
            "mrs {count}, CNTPCT_EL0",
            count = out(reg) count,
            options(nomem, nostack, preserves_flags)
        );
    }
    count
}

/// Loads one vCPU's EL1 virtual timer into the current processing element.
///
/// # Safety
///
/// The caller must prevent concurrent execution of this vCPU and must enter
/// the guest only after all of its architectural state has been restored.
pub unsafe fn activate_virtual_timer(context: &VirtualTimerContext) {
    // SAFETY: The caller exclusively owns the inactive vCPU and the EL12
    // virtual-timer bank selected by these encodings.
    unsafe {
        asm!(
            "msr S3_5_C14_C3_1, xzr",
            "msr CNTVOFF_EL2, {offset}",
            "msr S3_5_C14_C3_2, {compare_value}",
            "isb",
            "msr S3_5_C14_C3_1, {control}",
            "isb",
            offset = in(reg) context.offset(),
            compare_value = in(reg) context.compare_value(),
            control = in(reg) context.writable_control(),
            options(nostack, preserves_flags)
        );
    }
}

/// Saves and disables the current vCPU's EL1 virtual timer.
///
/// # Safety
///
/// Local IRQs must be masked, and `context` must identify the vCPU currently
/// loaded on this processing element.
pub unsafe fn deactivate_virtual_timer(context: &mut VirtualTimerContext) {
    let offset: u64;
    let compare_value: u64;
    let control: u64;
    // SAFETY: Guest execution is stopped, local IRQs are masked, and this
    // context exclusively owns the live EL12 timer bank.
    unsafe {
        asm!(
            "mrs {control}, S3_5_C14_C3_1",
            "mrs {compare_value}, S3_5_C14_C3_2",
            "mrs {offset}, CNTVOFF_EL2",
            "msr S3_5_C14_C3_1, xzr",
            "isb",
            "msr CNTVOFF_EL2, xzr",
            "isb",
            control = out(reg) control,
            compare_value = out(reg) compare_value,
            offset = out(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    context.restore_hardware_state(offset, compare_value, control);
}

/// Reports the live CNTV interrupt output on the current processing element.
pub fn virtual_timer_interrupt_asserted() -> bool {
    let control = read_virtual_timer_control();
    control & (registers::CNT_CTL_ENABLE | registers::CNT_CTL_IMASK | CONTROL_STATUS)
        == (registers::CNT_CTL_ENABLE | CONTROL_STATUS)
}

fn read_virtual_timer_control() -> u64 {
    let control: u64;
    // SAFETY: The VHE virtual timer control alias is readable at EL2 and has
    // no memory side effects.
    unsafe {
        asm!(
            "mrs {control}, S3_5_C14_C3_1",
            control = out(reg) control,
            options(nomem, nostack, preserves_flags)
        );
    }
    control
}

fn write_deadline(deadline: u64) {
    // SAFETY: CNTHP_CVAL_EL2 controls only the current CPU's EL2 timer.
    unsafe {
        asm!(
            "msr CNTHP_CVAL_EL2, {deadline}",
            "isb",
            deadline = in(reg) deadline,
            // Timer programming is observable by an interrupt handler, so it
            // also serves as a compiler barrier for surrounding queue state.
            options(nostack, preserves_flags)
        );
    }
}

fn write_control(control: u64) {
    // SAFETY: CNTHP_CTL_EL2 controls only the current CPU's EL2 timer.
    unsafe {
        asm!(
            "msr CNTHP_CTL_EL2, {control}",
            "isb",
            control = in(reg) control,
            options(nostack, preserves_flags)
        );
    }
}
