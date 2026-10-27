// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded, allocation-free event recording. Machine policy supplies CPU pinning
//! and timestamps; this mechanism never consults the scheduler or a clock.

#![no_std]

#[cfg(feature = "kernel-trace")]
mod global;
mod macros;
mod recorder;
#[cfg(feature = "kernel-trace")]
pub use global::{
    CAPACITY, MachineEmitter, emit, enabled, install, read, record_at, set_filter, start,
    start_with_capacity, stop,
};
pub use recorder::{Batch, Error, Recorder, Session, Ticket};

/// Independently selectable event groups. Values are filter bits, not priorities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Category {
    Scheduler = 1,
    Ipc = 2,
    Syscall = 4,
    Io = 8,
    Memory = 16,
    Irq = 32,
    Vm = 64,
    Test = 128,
}

/// A capture's category selection. Empty captures are rejected at start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Filter(u8);

impl Filter {
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self(u8::MAX);
    pub const fn only(category: Category) -> Self {
        Self(category as u8)
    }
    pub const fn with(self, category: Category) -> Self {
        Self(self.0 | category as u8)
    }
    pub const fn without(self, category: Category) -> Self {
        Self(self.0 & !(category as u8))
    }
}

/// Automatically emitted at each call site; strings are static and never copied
/// into the hot-path ring. References are kernel-local, not an export ABI.
#[derive(Debug)]
pub struct Site {
    pub category: Category,
    pub name: &'static str,
    pub fields: &'static [&'static str],
    pub file: &'static str,
    pub line: u32,
}

impl Site {
    pub const fn new(
        category: Category,
        name: &'static str,
        fields: &'static [&'static str],
        file: &'static str,
        line: u32,
    ) -> Self {
        assert!(
            fields.len() <= 4,
            "tracepoints support at most four scalar fields"
        );
        Self {
            category,
            name,
            fields,
            file,
            line,
        }
    }
}

/// One committed event. CPU and session identity belong to the read request;
/// sequence numbers are local to that CPU and capture, starting at zero.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub site: &'static Site,
    pub values: [u64; 4],
}

/// Pad scalar fields without allocating or accepting pointers/borrowed payloads.
pub fn values<const N: usize>(fields: [u64; N]) -> [u64; 4] {
    const { assert!(N <= 4, "tracepoints support at most four scalar fields") };
    let mut values = [0; 4];
    values[..N].copy_from_slice(&fields);
    values
}
