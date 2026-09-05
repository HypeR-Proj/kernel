// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral counter conversion and guest timer semantics.

use hyper::drivers::timer::arm_generic::{VirtualTimerState, VirtualTimerWake};
use hyper::hal::timer::{
    ConversionError, deadline_reached, nanoseconds_to_ticks, ticks_to_nanoseconds,
};
use hyper::time::counter_elapsed_microseconds;

#[test]
fn boot_counter_conversion_handles_uninitialized_and_wrapping_sources() {
    assert_eq!(counter_elapsed_microseconds(123, 100, 0), 0);
    assert_eq!(
        counter_elapsed_microseconds(24_012_000, 12_000, 24_000_000),
        1_000_000
    );
    assert_eq!(counter_elapsed_microseconds(4, u64::MAX - 5, 1_000_000), 10);
}

#[test]
fn converts_time_without_early_deadlines() {
    assert_eq!(crate::require_ok(nanoseconds_to_ticks(1, 24_000_000)), 1);
    assert_eq!(
        crate::require_ok(nanoseconds_to_ticks(1_000_000_000, 24_000_000)),
        24_000_000
    );
    assert_eq!(
        crate::require_ok(ticks_to_nanoseconds(24_000_000, 24_000_000)),
        1_000_000_000
    );
}

#[test]
fn rejects_invalid_or_unrepresentable_conversions() {
    assert_eq!(
        nanoseconds_to_ticks(1, 0),
        Err(ConversionError::InvalidFrequency)
    );
    assert_eq!(
        nanoseconds_to_ticks(u64::MAX, u64::MAX),
        Err(ConversionError::Overflow)
    );
    assert_eq!(
        ticks_to_nanoseconds(u64::MAX, 1),
        Err(ConversionError::Overflow)
    );
}

#[test]
fn compares_deadlines_across_counter_wraparound() {
    assert!(!deadline_reached(u64::MAX - 2, 1));
    assert!(deadline_reached(1, 1));
    assert!(deadline_reached(2, u64::MAX));
}

#[test]
fn models_guest_offset_masking_and_level_output() {
    let mut timer = VirtualTimerState::empty();
    timer.set_offset(1_000);
    timer.set_compare_value(250);
    timer.set_enabled(true);

    assert!(!timer.interrupt_asserted_at(1_249));
    assert!(timer.interrupt_asserted_at(1_250));
    timer.set_masked(true);
    assert!(!timer.interrupt_asserted_at(2_000));

    timer.restore_hardware_state(2_000, 10, 0b111);
    assert!(timer.enabled());
    assert!(timer.masked());
    assert_eq!(timer.writable_control(), 0b11);
}

#[test]
fn derives_wfi_wake_deadline_from_saved_virtual_timer_state() {
    let mut timer = VirtualTimerState::empty();
    assert_eq!(timer.wfi_wake_at(100), VirtualTimerWake::None);
    timer.set_offset(1_000);
    timer.set_compare_value(250);
    timer.set_enabled(true);
    assert_eq!(timer.wfi_wake_at(1_249), VirtualTimerWake::Deadline(1_250));
    assert_eq!(timer.wfi_wake_at(1_250), VirtualTimerWake::PendingNow);
    timer.set_offset(0);
    timer.set_compare_value(100_u64.wrapping_add(1_u64 << 63));
    assert_eq!(timer.wfi_wake_at(100), VirtualTimerWake::PendingNow);
    timer.set_masked(true);
    assert_eq!(timer.wfi_wake_at(2_000), VirtualTimerWake::None);
}
