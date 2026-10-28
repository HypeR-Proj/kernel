// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{Category, Filter, Record, Site};
use alloc::vec::Vec;
use hyper_core::sync::{
    SpinLock,
    atomic::{AtomicU64, Ordering},
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
    Allocation,
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

struct Ring {
    next: u64,
    records: Vec<Option<Record>>,
}
impl Ring {
    const fn new() -> Self {
        Self {
            next: 0,
            records: Vec::new(),
        }
    }
}

#[repr(align(64))]
struct Cpu {
    ring: SpinLock<Ring>,
    // generation << 2 | frozen | loss. Stop freezes metadata as well as records;
    // delayed rejected producers must not change an exported or replaced run.
    loss: AtomicU64,
}
impl Cpu {
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
    present: bool,
    retirement: Vec<Ring>,
}

/// Dynamically prepared CPU-local rings. Every lock acquisition is a single try;
/// producers never spin, allocate or acquire the control lock. Readers and
/// producers share a ring lock, including recursive interrupt-context calls.
///
/// Callbacks passed to `record` must be short, nonblocking scalar expressions.
/// Control/read operations are for ordinary thread context. The machine adapter
/// pins producers to the CPU whose ring they select.
pub struct Recorder<const CPUS: usize, const CAPACITY: usize> {
    requested: AtomicU64,
    control: SpinLock<Control>,
    cpus: [Cpu; CPUS],
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
            control: SpinLock::new(Control {
                generation: 0,
                stopped: true,
                present: false,
                retirement: Vec::new(),
            }),
            cpus: [const { Cpu::new() }; CPUS],
        }
    }

    /// Allocates payloads and retirement metadata outside recorder locks. Publish
    /// swaps ownership only; drop the preparation after releasing enclosing locks.
    pub fn prepare(
        &self,
        capacity: usize,
        selected: impl Fn(usize) -> bool,
    ) -> Result<PreparedCapture<'_, CPUS, CAPACITY>, Error> {
        if capacity == 0 || capacity > CAPACITY {
            return Err(Error::InvalidFilter);
        }
        self.prepare_storage(capacity, selected)
    }

    fn prepare_storage(
        &self,
        capacity: usize,
        selected: impl Fn(usize) -> bool,
    ) -> Result<PreparedCapture<'_, CPUS, CAPACITY>, Error> {
        let mut rings = Vec::new();
        rings
            .try_reserve_exact(CPUS)
            .map_err(|_| Error::Allocation)?;
        let mut retirement = Vec::new();
        retirement
            .try_reserve_exact(CPUS)
            .map_err(|_| Error::Allocation)?;
        for cpu in 0..CPUS {
            let mut ring = Ring::new();
            if selected(cpu) {
                ring.records
                    .try_reserve_exact(capacity)
                    .map_err(|_| Error::Allocation)?;
                ring.records.resize(capacity, None);
            }
            rings.push(ring);
            retirement.push(Ring::new());
        }
        Ok(PreparedCapture {
            recorder: self,
            rings,
            retirement,
            capacity,
            consumed: false,
        })
    }

    /// Convenience for ordinary thread context without an enclosing control lock.
    pub fn start(&self, filter: Filter) -> Result<Session, Error> {
        self.start_with_capacity(filter, CAPACITY)
    }

    pub fn start_with_capacity(&self, filter: Filter, capacity: usize) -> Result<Session, Error> {
        self.prepare(capacity, |_| true)?.start(filter)
    }

    /// Release a stopped capture; its generation remains permanently retired.
    pub fn clear(&self, session: Session) -> Result<(), Error> {
        drop(self.detach(session)?);
        Ok(())
    }

    /// Detaches a stopped capture without allocating. Drop the returned owner
    /// after releasing any enclosing control locks to reclaim its storage.
    pub fn detach(&self, session: Session) -> Result<RetiredCapture, Error> {
        self.control
            .try_with(|control| {
                if !control.present || control.generation != session.0 {
                    return Err(Error::InvalidSession);
                }
                if !control.stopped {
                    return Err(Error::Busy);
                }
                let locks: [_; CPUS] = core::array::from_fn(|cpu| &self.cpus[cpu].ring);
                let mut index = 0;
                if !SpinLock::try_with_each(&locks, |ring| {
                    core::mem::swap(ring, &mut control.retirement[index]);
                    index += 1;
                }) {
                    return Err(Error::Busy);
                }
                control.present = false;
                Ok(RetiredCapture {
                    _rings: core::mem::take(&mut control.retirement),
                })
            })
            .ok_or(Error::Busy)?
    }

    /// After acquiring control, disable admission and join writers without
    /// spinning. BUSY means retry; start stays forbidden until successful stop.
    pub fn stop(&self, session: Session) -> Result<(), Error> {
        self.control
            .try_with(|control| {
                if !control.present || control.generation != session.0 {
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
                if !control.present || control.generation != session.0 {
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
                if self.requested.load(Ordering::Acquire) != ticket.0 || ring.records.is_empty() {
                    return;
                }
                let Some(timestamp_ns) = clock() else {
                    cpu.lost(generation);
                    return;
                };
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
                let index = ring.next as usize % ring.records.len();
                ring.records[index] = Some(record);
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
                if !control.present || control.generation != session.0 {
                    return Err(Error::InvalidSession);
                }
                cpu.ring
                    .try_with(|ring| {
                        let end = ring.next;
                        let oldest = end.saturating_sub(ring.records.len() as u64);
                        let first = cursor.max(oldest);
                        let count = end
                            .saturating_sub(first)
                            .min(out.len().min(READ_BATCH) as u64)
                            as usize;
                        for (index, dst) in out.iter_mut().take(count).enumerate() {
                            *dst = ring.records[(first as usize + index) % ring.records.len()];
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

/// Detached storage. Its destructor reclaims payloads and retirement metadata;
/// callers holding outer locks must move this owner out before dropping it.
#[must_use = "drop retired storage outside enclosing control locks"]
pub struct RetiredCapture {
    _rings: Vec<Ring>,
}

/// Prepared payloads and empty retirement slots. Replaced storage retires on Drop.
pub struct PreparedCapture<'a, const CPUS: usize, const CAPACITY: usize> {
    recorder: &'a Recorder<CPUS, CAPACITY>,
    rings: Vec<Ring>,
    retirement: Vec<Ring>,
    capacity: usize,
    consumed: bool,
}

impl<const CPUS: usize, const CAPACITY: usize> PreparedCapture<'_, CPUS, CAPACITY> {
    /// Publish all selected rings atomically with admission. A failed start leaves
    /// both the old capture and this preparation intact, ready for retry or Drop.
    pub fn start(&mut self, filter: Filter) -> Result<Session, Error> {
        if self.consumed || self.capacity == 0 || filter.0 == 0 {
            return Err(Error::InvalidFilter);
        }
        self.recorder
            .control
            .try_with(|control| {
                if !control.stopped {
                    return Err(Error::Busy);
                }
                let generation = control
                    .generation
                    .checked_add(1)
                    .filter(|value| *value <= MAX_GENERATION)
                    .ok_or(Error::Exhausted)?;
                let locks: [_; CPUS] = core::array::from_fn(|cpu| &self.recorder.cpus[cpu].ring);
                let mut index = 0;
                if !SpinLock::try_with_each(&locks, |ring| {
                    core::mem::swap(ring, &mut self.rings[index]);
                    self.recorder.cpus[index]
                        .loss
                        .store(generation << 2, Ordering::Relaxed);
                    index += 1;
                }) {
                    return Err(Error::Busy);
                }
                // Retain fresh empty slots for allocation-free clear. Any previous
                // metadata leaves through this preparation, outside control locks.
                core::mem::swap(&mut control.retirement, &mut self.retirement);
                self.consumed = true;
                control.generation = generation;
                control.stopped = false;
                control.present = true;
                self.recorder.requested.store(
                    (generation << FILTER_BITS) | u64::from(filter.0),
                    Ordering::Release,
                );
                Ok(Session(generation))
            })
            .ok_or(Error::Busy)?
    }
}
