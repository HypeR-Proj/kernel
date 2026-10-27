// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest timer comparisons and bounded host waits in architectural time ticks.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerWake {
    Disabled,
    PendingNow,
    /// Recheck the architectural comparison after this duration. A capped
    /// duration is a checkpoint, never permission to inject an interrupt early.
    AfterTicks(u64),
}

/// Sstc compares the unsigned, wrapping sum of TIME and HTIMEDELTA against
/// VSTIMECMP. Global interrupt enable does not participate in WFI wakeup.
pub const fn timer_wake(physical: u64, offset: u64, compare: u64, enabled: bool) -> TimerWake {
    if !enabled {
        return TimerWake::Disabled;
    }
    let guest = physical.wrapping_add(offset);
    if guest >= compare {
        TimerWake::PendingNow
    } else {
        let remaining = compare - guest;
        TimerWake::AfterTicks(if remaining > i64::MAX as u64 {
            i64::MAX as u64
        } else {
            remaining
        })
    }
}
