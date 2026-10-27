// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

mod buffer;
mod drain;
mod output;

pub use buffer::{
    AppendError, Level, ReadError, ReadResult, Record, RecordFlags, RingBuffer, Timestamp,
};
pub use drain::{
    ByteRing, DrainBarrierError, DrainBarrierRegistration, DrainBarrierSet, DrainBarrierStatus,
    DrainBarrierToken,
};
// Compatibility names keep the logging API stable while the implementation
// lives in the architecture-neutral synchronization layer.
pub use output::{
    EmergencyQuiescence, EmergencyWriteGate, OutputBuffer, OutputError, OutputProgress,
    RuntimeByteAccess, RuntimeBytePermit,
};
pub use {
    hyper::sync::DeferredWork as DeferredDrain, hyper::sync::WorkDisposition as DrainDisposition,
};
