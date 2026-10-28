// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Machine-independent global capture. Only the enabled path calls the immutable
//! machine adapter; the disabled fast path is a single category/generation load.

use super::{
    Batch, Category, Error, Filter, PreparedCapture, Record, Recorder, RetiredCapture, Session,
    Site, Ticket,
};
use hyper_core::sync::PublishedOnce;

/// The adapter pins execution and obtains the CPU/clock before calling `record_at`.
/// Fields borrow the caller's stack, allocate nothing and are evaluated at most
/// once. The adapter must not retain them or call `emit` recursively.
pub type MachineEmitter = fn(Ticket, &'static Site, &mut dyn FnMut() -> [u64; 4]);

pub const CAPACITY: usize = 16384;
static RECORDER: Recorder<{ hyper_core::cpu::MAX_CPUS }, CAPACITY> = Recorder::new();
static MACHINE: PublishedOnce<MachineEmitter> = PublishedOnce::new();

pub fn prepare(
    capacity: usize,
    selected: impl Fn(usize) -> bool,
) -> Result<PreparedCapture<'static, { hyper_core::cpu::MAX_CPUS }, CAPACITY>, Error> {
    if MACHINE.get().is_none() {
        return Err(Error::Unavailable);
    }
    RECORDER.prepare(capacity, selected)
}
pub fn detach(session: Session) -> Result<RetiredCapture, Error> {
    RECORDER.detach(session)
}

/// Publish the sole machine adapter before allowing capture. Replacing the
/// adapter is rejected; this changes data once, never executable instructions.
pub fn install(emitter: MachineEmitter) -> Result<(), Error> {
    MACHINE
        .publish(emitter)
        .map_err(|_| Error::AlreadyInitialized)
}

pub fn start(filter: Filter) -> Result<Session, Error> {
    if MACHINE.get().is_none() {
        return Err(Error::Unavailable);
    }
    RECORDER.start(filter)
}
pub fn start_with_capacity(filter: Filter, capacity: usize) -> Result<Session, Error> {
    if MACHINE.get().is_none() {
        return Err(Error::Unavailable);
    }
    RECORDER.start_with_capacity(filter, capacity)
}
pub fn stop(session: Session) -> Result<(), Error> {
    RECORDER.stop(session)
}
pub fn clear(session: Session) -> Result<(), Error> {
    RECORDER.clear(session)
}
pub fn set_filter(session: Session, filter: Filter) -> Result<(), Error> {
    RECORDER.set_filter(session, filter)
}
pub fn read(
    session: Session,
    cpu: usize,
    cursor: u64,
    out: &mut [Option<Record>],
) -> Result<Batch, Error> {
    RECORDER.read(session, cpu, cursor, out)
}

#[inline]
pub fn enabled(category: Category) -> Option<Ticket> {
    RECORDER.enabled(category)
}

#[doc(hidden)]
pub fn emit(ticket: Ticket, site: &'static Site, mut fields: impl FnMut() -> [u64; 4]) {
    if let Some(emitter) = MACHINE.get() {
        emitter(ticket, site, &mut fields);
    }
}

/// Machine-adapter entry: CPU selection, timestamp and append must all run in
/// one non-migrating scope. No scheduler, HAL or policy dependency lives here.
pub fn record_at(
    ticket: Ticket,
    cpu: usize,
    site: &'static Site,
    clock: impl FnOnce() -> Option<u64>,
    fields: impl FnOnce() -> [u64; 4],
) {
    RECORDER.record(ticket, cpu, site, clock, fields);
}
