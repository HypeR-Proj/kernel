// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Normalized UTC timestamps, independent of a physical clock source.

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    pub const fn new(seconds: i64, nanoseconds: u32) -> Option<Self> {
        if nanoseconds >= 1_000_000_000 {
            return None;
        }
        Some(Self {
            seconds,
            nanoseconds,
        })
    }

    pub const fn seconds(self) -> i64 {
        self.seconds
    }
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    pub fn checked_add_nanoseconds(self, delta: u64) -> Option<Self> {
        let nanos = u64::from(self.nanoseconds) + delta % 1_000_000_000;
        let seconds = i64::try_from(delta / 1_000_000_000 + nanos / 1_000_000_000).ok()?;
        Self::new(
            self.seconds.checked_add(seconds)?,
            (nanos % 1_000_000_000) as u32,
        )
    }
}
