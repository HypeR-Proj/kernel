// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded retention admission, independent of object and scheduler ownership.

pub(super) const MAX_ARMED: usize = 64;
pub(super) const MAX_RETAINED: usize = 4;
pub(super) const MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_TIMEOUT_NS: u64 = 60_000_000_000;

pub(super) const fn valid_request(bytes: u64, timeout_ns: u64) -> bool {
    bytes <= MAX_BYTES && timeout_ns != 0 && timeout_ns <= MAX_TIMEOUT_NS
}

#[derive(Default)]
pub(super) struct Budget {
    armed: usize,
    retained: usize,
    bytes: u64,
}

impl Budget {
    pub(super) fn arm(&mut self) -> bool {
        if self.armed == MAX_ARMED {
            return false;
        }
        self.armed += 1;
        true
    }
    pub(super) fn disarm(&mut self) {
        self.armed -= 1;
    }
    pub(super) fn retain(&mut self, requested: u64) -> Option<u64> {
        if self.retained == MAX_RETAINED {
            return None;
        }
        let admitted = requested.min(MAX_BYTES - self.bytes);
        self.retained += 1;
        self.bytes += admitted;
        Some(admitted)
    }
    pub(super) fn release(&mut self, bytes: u64) {
        self.retained -= 1;
        self.bytes -= bytes;
    }
    pub(super) fn commit_bytes(&mut self, reserved: u64, actual: u64) {
        debug_assert!(actual <= reserved);
        self.bytes -= reserved - actual;
    }
}
