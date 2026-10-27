// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Goldfish virtual hardware RTC register interface.
//! `TIME_LOW` latches `TIME_HIGH`; together they hold signed UTC nanoseconds.

pub const TIME_LOW: usize = 0x00;
pub const TIME_HIGH: usize = 0x04;
pub const REGISTER_WINDOW: u64 = 0x08;
