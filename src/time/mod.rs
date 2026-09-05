// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent software time mechanisms.

mod owned_queue;
mod queue;

pub use owned_queue::{
    ExpiredTimer, OwnedDeadlineQueue, PendingReservedTimer, PendingTimer, ReservedTimerCallbacks,
    ReservedTimerNode, TimerRecycle,
};
pub use queue::{
    DeadlineQueue, Error as TimerQueueError, QueueStats, TimerCallback, TimerEvent, TimerHandle,
    TimerMode,
};

/// Converts a wrapping hardware-counter delta to elapsed microseconds.
///
/// A zero frequency represents an uninitialized clocksource and produces zero.
pub fn counter_elapsed_microseconds(now: u64, origin: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }
    let elapsed = now.wrapping_sub(origin);
    let microseconds = u128::from(elapsed) * 1_000_000 / u128::from(frequency_hz);
    microseconds.min(u128::from(u64::MAX)) as u64
}
