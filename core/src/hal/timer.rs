// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionError {
    InvalidFrequency,
    Overflow,
}

/// A system-wide monotonically increasing hardware counter.
pub trait MonotonicCounter {
    type Error;

    fn frequency_hz() -> Result<u64, Self::Error>;
    fn read() -> u64;
}

/// A per-CPU timer programmed with an absolute counter deadline.
pub trait DeadlineTimer {
    type Error;

    fn set_deadline(deadline: u64) -> Result<(), Self::Error>;
    fn mask();
    fn disable();
}

/// Converts nanoseconds to counter ticks, rounding up so a deadline never
/// expires earlier than requested.
pub fn nanoseconds_to_ticks(nanoseconds: u64, frequency_hz: u64) -> Result<u64, ConversionError> {
    if frequency_hz == 0 {
        return Err(ConversionError::InvalidFrequency);
    }
    let product = u128::from(nanoseconds)
        .checked_mul(u128::from(frequency_hz))
        .ok_or(ConversionError::Overflow)?;
    let ticks = product
        .checked_add(999_999_999)
        .ok_or(ConversionError::Overflow)?
        / 1_000_000_000;
    u64::try_from(ticks).map_err(|_| ConversionError::Overflow)
}

/// Converts counter ticks to elapsed nanoseconds, rounding down.
pub fn ticks_to_nanoseconds(ticks: u64, frequency_hz: u64) -> Result<u64, ConversionError> {
    if frequency_hz == 0 {
        return Err(ConversionError::InvalidFrequency);
    }
    let nanoseconds = u128::from(ticks)
        .checked_mul(1_000_000_000)
        .ok_or(ConversionError::Overflow)?
        / u128::from(frequency_hz);
    u64::try_from(nanoseconds).map_err(|_| ConversionError::Overflow)
}

/// Compares wrapping counter values for deadlines less than half a counter
/// period apart.
pub const fn deadline_reached(current: u64, deadline: u64) -> bool {
    (current.wrapping_sub(deadline) as i64) >= 0
}
