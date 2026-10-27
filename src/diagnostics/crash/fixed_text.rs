// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free text whose initialized bytes are always a source prefix.

use core::fmt::{self, Write};

/// Fixed-capacity UTF-8 text used only while assembling crash diagnostics.
///
/// The first truncated fragment closes the value permanently. Ignoring later
/// fragments is essential when truncation backs up to a UTF-8 boundary: using
/// the leftover bytes would splice later text into the middle of the source.
pub(super) struct FixedText<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    length: usize,
    truncated: bool,
}

impl<const CAPACITY: usize> FixedText<CAPACITY> {
    pub(super) fn capture(arguments: fmt::Arguments<'_>) -> Self {
        let mut text = Self::new();
        let _ = text.write_fmt(arguments);
        text
    }

    pub(super) const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
            truncated: false,
        }
    }

    pub(super) fn as_str(&self) -> &str {
        // fmt::Write accepts only UTF-8 and write_str preserves character
        // boundaries, so the initialized prefix is always valid UTF-8.
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }

    pub(super) const fn was_truncated(&self) -> bool {
        self.truncated
    }
}

impl<const CAPACITY: usize> fmt::Write for FixedText<CAPACITY> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }

        let available = CAPACITY - self.length;
        let mut copied = available.min(value.len());
        while !value.is_char_boundary(copied) {
            copied -= 1;
        }
        self.bytes[self.length..self.length + copied].copy_from_slice(&value.as_bytes()[..copied]);
        self.length += copied;
        self.truncated = copied != value.len();
        Ok(())
    }
}
