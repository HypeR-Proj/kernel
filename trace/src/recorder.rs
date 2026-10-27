// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{Category, Filter, Record, Site};
use hyper_core::sync::{
    SpinLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

const FILTER_BITS: u32 = 8;
const MAX_GENERATION: u64 = u64::MAX >> FILTER_BITS;
const LOSS: u64 = 1;
const FROZEN: u64 = 2;
/// Upper bound on records copied while a reader owns a CPU's ring.
const READ_BATCH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyInitialized,
    Unavailable,
    Busy,
    InvalidSession,
    InvalidCpu,
    InvalidFilter,
    NotRecording,
    Exhausted,
}

/// A generation-qualified control/read token, never reused by this recorder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session(u64);
impl Session {
    pub const fn generation(self) -> u64 {
        self.0
    }
}

/// A cheap admission snapshot. Recording revalidates it under the CPU lock so
/// delayed callers cannot publish into a replacement session.
#[derive(Clone, Copy)]
pub struct Ticket(u64);

impl Ticket {
    /// Capture identity for explicit request contexts; never an authority token.
    pub const fn generation(self) -> u64 {
        self.0 >> FILTER_BITS
    }
}

#[derive(Debug)]
pub struct Batch {
    pub count: usize,
    pub next: u64,
    /// Exact number of committed records overwritten before this cursor.
    pub overwritten: u64,
    /// At least one event was lost to contention, recursion or a missing clock.
    /// Deliberately a flag: counting recursive producers would require retries.
    pub incomplete: bool,
}

struct Ring<const N: usize> {
    generation: u64,
    next: u64,
    records: [Option<Record>; N],
    capacity: usize,
}
impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self {
            generation: 0,
            next: 0,
            records: [None; N],
            capacity: N,
        }
    }
}

#[repr(align(64))]
struct Cpu<const N: usize> {
    ring: SpinLock<Ring<N>>,
    // generation << 2 | frozen | loss. Stop freezes metadata as well as records;
    // delayed rejected producers must not change an exported or replaced run.
    loss: AtomicU64,
}
impl<const N: usize> Cpu<N> {
    const fn new() -> Self {
        Self {
            ring: SpinLock::new(Ring::new()),
            loss: AtomicU64::new(0),
        }
    }
    fn lost(&self, generation: u64) {
        let _ = self.loss.compare_exchange(
            generation << 2,
            (generation << 2) | LOSS,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

struct Control {
    generation: u64,
    stopped: bool,
}

/// Fixed storage for CPU-local rings. Every lock acquisition is a single try;
/// producers never spin, allocate or acquire the control lock. Readers and
/// producers share a ring lock, including recursive interrupt-context calls.
///
/// Callbacks passed to `record` must be short, nonblocking scalar expressions.
/// Control/read operations are for ordinary thread context. The machine adapter
/// pins producers to the CPU whose ring they select.
pub struct Recorder<const CPUS: usize, const CAPACITY: usize> {
    requested: AtomicU64,
    capacity: AtomicUsize,
    control: SpinLock<Control>,
    cpus: [Cpu<CAPACITY>; CPUS],
}

impl<const CPUS: usize, const CAPACITY: usize> Default for Recorder<CPUS, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CPUS: usize, const CAPACITY: usize> Recorder<CPUS, CAPACITY> {
    pub const fn new() -> Self {
        assert!(CPUS > 0 && CAPACITY > 0);
        Self {
            requested: AtomicU64::new(0),
            capacity: AtomicUsize::new(CAPACITY),
            control: SpinLock::new(Control {
                generation: 0,
                stopped: true,
            }),
            cpus: [const { Cpu::new() }; CPUS],
        }
    }

    /// Start explicitly discards the previous capture. Rings reset lazily on
    /// their first new event, avoiding a large clear in the control path.
    pub fn start(&self, filter: Filter) -> Result<Session, Error> {
        self.start_with_capacity(filter, CAPACITY)
    }

    /// Selects a bounded overwrite window without allocating in producer context.
    pub fn start_with_capacity(&self, filter: Filter, capacity: usize) -> Result<Session, Error> {
        if filter.0 == 0 || capacity == 0 || capacity > CAPACITY {
            return Err(Error::InvalidFilter);
        }
        self.control
            .try_with(|control| {
                if !control.stopped {
                    return Err(Error::Busy);
                }
                let generation = control
                    .generation
                    .checked_add(1)
                    .filter(|value| *value <= MAX_GENERATION)
                    .ok_or(Error::Exhausted)?;
                for cpu in &self.cpus {
                    cpu.loss.store(generation << 2, Ordering::Relaxed);
                }
                self.capacity.store(capacity, Ordering::Relaxed);
                control.generation = generation;
                control.stopped = false;
                // Publishes the initialized loss stamps and the new filter together.
                self.requested.store(
                    (generation << FILTER_BITS) | u64::from(filter.0),
                    Ordering::Release,
                );
                Ok(Session(generation))
            })
            .ok_or(Error::Busy)?
    }

    /// After acquiring control, disable admission and join writers without
    /// spinning. BUSY means retry; start stays forbidden until successful stop.
    pub fn stop(&self, session: Session) -> Result<(), Error> {
        self.control
            .try_with(|control| {
                if control.generation != session.0 {
                    return Err(Error::InvalidSession);
                }
                self.requested.store(0, Ordering::Release);
                // An admitted writer must release its ring before this acquire.
                // A later holder acquires our release, so its gate recheck cannot
                // see the pre-stop request. No global producer counter is needed.
                for cpu in &self.cpus {
                    cpu.ring
                        .try_with(|_| {
                            cpu.loss.fetch_or(FROZEN, Ordering::Relaxed);
                        })
                        .ok_or(Error::Busy)?;
                }
                control.stopped = true;
                Ok(())
            })
            .ok_or(Error::Busy)?
    }

    /// Change categories without discarding records. NONE pauses new admission;
    /// an already admitted writer can finish. Only stop joins those writers.
    pub fn set_filter(&self, session: Session, filter: Filter) -> Result<(), Error> {
        self.control
            .try_with(|control| {
                if control.generation != session.0 {
                    return Err(Error::InvalidSession);
                }
                if control.stopped {
                    return Err(Error::NotRecording);
                }
                if self.requested.load(Ordering::Acquire) == 0 {
                    return Err(Error::Busy);
                }
                self.requested.store(
                    (session.0 << FILTER_BITS) | u64::from(filter.0),
                    Ordering::Release,
                );
                Ok(())
            })
            .ok_or(Error::Busy)?
    }

    /// Disabled tracepoints perform only this load/bit test, before evaluating
    /// payloads, disabling interrupts or reading the clock.
    #[inline]
    pub fn enabled(&self, category: Category) -> Option<Ticket> {
        let state = self.requested.load(Ordering::Acquire);
        (state & category as u64 != 0).then_some(Ticket(state))
    }

    pub fn record(
        &self,
        ticket: Ticket,
        cpu: usize,
        site: &'static Site,
        clock: impl FnOnce() -> Option<u64>,
        fields: impl FnOnce() -> [u64; 4],
    ) {
        let Some(cpu) = self.cpus.get(cpu) else {
            return;
        };
        let generation = ticket.0 >> FILTER_BITS;
        if ticket.0 & site.category as u64 == 0 {
            return;
        }
        if cpu
            .ring
            .try_with(|ring| {
                if self.requested.load(Ordering::Acquire) != ticket.0 {
                    return;
                }
                let Some(timestamp_ns) = clock() else {
                    cpu.lost(generation);
                    return;
                };
                if ring.generation != generation {
                    ring.generation = generation;
                    ring.next = 0;
                    ring.capacity = self.capacity.load(Ordering::Relaxed);
                }
                let Some(next) = ring.next.checked_add(1) else {
                    cpu.lost(generation);
                    return;
                };
                let record = Record {
                    sequence: ring.next,
                    timestamp_ns,
                    site,
                    values: fields(),
                };
                ring.records[ring.next as usize % ring.capacity] = Some(record);
                ring.next = next;
            })
            .is_none()
        {
            cpu.lost(generation);
        }
    }

    /// Non-destructive, bounded batch read. Inspect only `out[..batch.count]`.
    /// A zero-count batch is not EOF unless stop has completed. Busy is retryable.
    pub fn read(
        &self,
        session: Session,
        cpu: usize,
        cursor: u64,
        out: &mut [Option<Record>],
    ) -> Result<Batch, Error> {
        let cpu = self.cpus.get(cpu).ok_or(Error::InvalidCpu)?;
        self.control
            .try_with(|control| {
                if control.generation != session.0 {
                    return Err(Error::InvalidSession);
                }
                cpu.ring
                    .try_with(|ring| {
                        let end = if ring.generation == session.0 {
                            ring.next
                        } else {
                            0
                        };
                        let oldest = end.saturating_sub(ring.capacity as u64);
                        let first = cursor.max(oldest);
                        let count = end
                            .saturating_sub(first)
                            .min(out.len().min(READ_BATCH) as u64)
                            as usize;
                        for (index, dst) in out.iter_mut().take(count).enumerate() {
                            *dst = ring.records[(first as usize + index) % ring.capacity];
                        }
                        Batch {
                            count,
                            next: first + count as u64,
                            overwritten: oldest.saturating_sub(cursor),
                            incomplete: cpu.loss.load(Ordering::Relaxed) & LOSS != 0,
                        }
                    })
                    .ok_or(Error::Busy)
            })
            .ok_or(Error::Busy)?
    }
}
