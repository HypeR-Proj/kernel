// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scalar observations at committed switch selection; never scheduler policy.

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum SwitchReason {
    Yield = 1,
    Preempt = 2,
    Block = 3,
    Exit = 4,
    Migrate = 5,
    Idle = 6,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum SchedulePoint {
    Explicit = 1,
    SafePoint = 2,
    IrqTail = 3,
}

/// Two byte-sized enums share a scalar so a switch needs only one ring record.
#[cfg(feature = "kernel-trace")]
pub(super) fn switch_state(previous: u64, next_class: u64) -> u64 {
    previous | (next_class << 8)
}

#[cfg(feature = "kernel-trace")]
pub(super) fn switch_cause(reason: SwitchReason, point: SchedulePoint) -> u64 {
    reason as u64 | ((point as u64) << 8)
}
