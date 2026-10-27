// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `PrimeCell` PL031 register offsets (ARM DDI 0224C, chapter 3).

pub const DATA: usize = 0x000;
pub const CONTROL: usize = 0x00c;
pub const ENABLED: u32 = 1;
pub const REGISTER_WINDOW: u64 = 0x010;
