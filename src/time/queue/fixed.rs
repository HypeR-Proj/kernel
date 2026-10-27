// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-capacity indexed deadline heap.

use hyper::hal::timer::deadline_reached;
use {hyper::collections::indexed_heap, hyper::collections::indexed_heap::Storage};

const NONE: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    Full,
    InvalidHandle,
    InvalidInterval,
    QueueAlreadyUsed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerMode {
    OneShot,
    Periodic { interval: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerHandle {
    queue_id: usize,
    slot: usize,
    generation: u64,
}

impl TimerHandle {
    pub(super) const fn owned(queue_id: usize, identity: u64) -> Self {
        Self {
            queue_id,
            slot: NONE,
            generation: identity,
        }
    }

    pub(super) const fn owned_identity(self, queue_id: usize) -> Option<u64> {
        if self.queue_id == queue_id && self.slot == NONE && self.generation != 0 {
            Some(self.generation)
        } else {
            None
        }
    }

    /// Returns the identity of the queue that owns this timer.
    ///
    /// Queue ownership is stable for the lifetime of a handle. Higher-level
    /// per-CPU timer services use it to retire a timer after the requesting
    /// thread has migrated away from the CPU on which the timer was armed.
    pub const fn queue_id(self) -> usize {
        self.queue_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerEvent {
    pub handle: TimerHandle,
    pub deadline: u64,
    pub observed_at: u64,
    /// Periods skipped because interrupt handling ran late.
    pub overruns: u64,
}

pub type TimerCallback = fn(TimerEvent, usize);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueueStats {
    pub active_timers: usize,
    pub peak_timers: usize,
    pub schedules: u64,
    pub schedule_failures: u64,
    pub cancellations: u64,
    pub reschedules: u64,
    pub callbacks: u64,
    pub overruns: u64,
}

#[derive(Clone, Copy)]
struct Slot {
    occupied: bool,
    generation: u64,
    heap_index: usize,
    deadline: u64,
    sequence: u64,
    mode: TimerMode,
    callback: TimerCallback,
    context: usize,
}

impl Slot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 0,
        heap_index: NONE,
        deadline: 0,
        sequence: 0,
        mode: TimerMode::OneShot,
        callback: empty_callback,
        context: 0,
    };
}

/// Multiple logical timers ordered behind one hardware deadline comparator.
///
/// Deadlines in one queue must remain within half of the wrapping counter
/// range, matching [`deadline_reached`].
pub struct DeadlineQueue<const CAPACITY: usize> {
    queue_id: usize,
    slots: [Slot; CAPACITY],
    heap: [usize; CAPACITY],
    heap_len: usize,
    free_slots: [usize; CAPACITY],
    free_len: usize,
    next_sequence: u64,
    stats: QueueStats,
}

impl<const CAPACITY: usize> DeadlineQueue<CAPACITY> {
    pub const fn new() -> Self {
        Self::with_id(0)
    }

    /// Creates a queue whose handles cannot be used with a different queue ID.
    pub const fn with_id(queue_id: usize) -> Self {
        let mut free_slots = [0; CAPACITY];
        let mut index = 0;
        while index < CAPACITY {
            free_slots[index] = index;
            index += 1;
        }
        Self {
            queue_id,
            slots: [Slot::EMPTY; CAPACITY],
            heap: [0; CAPACITY],
            heap_len: 0,
            free_slots,
            free_len: CAPACITY,
            next_sequence: 0,
            stats: QueueStats {
                active_timers: 0,
                peak_timers: 0,
                schedules: 0,
                schedule_failures: 0,
                cancellations: 0,
                reschedules: 0,
                callbacks: 0,
                overruns: 0,
            },
        }
    }

    /// Assigns an identity to a statically constructed queue before first use.
    ///
    /// This avoids materializing a potentially large fixed-capacity queue on
    /// a kernel stack merely to change its ID. Once a scheduling operation has
    /// been attempted, rebinding is rejected so existing handles cannot alias
    /// another queue identity.
    pub fn initialize_id(&mut self, queue_id: usize) -> Result<(), Error> {
        if self.stats.schedules != 0 || self.heap_len != 0 {
            return Err(Error::QueueAlreadyUsed);
        }
        self.queue_id = queue_id;
        Ok(())
    }

    pub fn schedule(
        &mut self,
        deadline: u64,
        mode: TimerMode,
        callback: TimerCallback,
        context: usize,
    ) -> Result<TimerHandle, Error> {
        self.stats.schedules = self.stats.schedules.saturating_add(1);
        if matches!(
            mode,
            TimerMode::Periodic {
                interval: 0 | 0x8000_0000_0000_0000..
            }
        ) {
            return self.schedule_error(Error::InvalidInterval);
        }
        let Some(free_index) = self.free_len.checked_sub(1) else {
            return self.schedule_error(Error::Full);
        };
        self.free_len = free_index;
        let slot_index = self.free_slots[free_index];
        let generation = next_generation(self.slots[slot_index].generation);
        let sequence = self.take_sequence();
        self.slots[slot_index] = Slot {
            occupied: true,
            generation,
            heap_index: NONE,
            deadline,
            sequence,
            mode,
            callback,
            context,
        };
        self.heap_insert(slot_index);
        self.refresh_active_stats();
        Ok(TimerHandle {
            queue_id: self.queue_id,
            slot: slot_index,
            generation,
        })
    }

    pub fn cancel(&mut self, handle: TimerHandle) -> Result<(), Error> {
        let slot_index = self.validate_handle(handle)?;
        let heap_index = self.slots[slot_index].heap_index;
        self.heap_remove(heap_index);
        self.release_slot(slot_index);
        self.stats.cancellations = self.stats.cancellations.saturating_add(1);
        self.refresh_active_stats();
        Ok(())
    }

    pub fn reschedule(&mut self, handle: TimerHandle, deadline: u64) -> Result<(), Error> {
        let slot_index = self.validate_handle(handle)?;
        let heap_index = self.slots[slot_index].heap_index;
        self.heap_remove(heap_index);
        self.slots[slot_index].deadline = deadline;
        self.slots[slot_index].sequence = self.take_sequence();
        self.heap_insert(slot_index);
        self.stats.reschedules = self.stats.reschedules.saturating_add(1);
        Ok(())
    }

    pub fn pop_expired(&mut self, now: u64) -> Option<(TimerEvent, TimerCallback, usize)> {
        if self.heap_len == 0 {
            return None;
        }
        let slot_index = self.heap[0];
        let deadline = self.slots[slot_index].deadline;
        if !deadline_reached(now, deadline) {
            return None;
        }
        let slot = self.slots[slot_index];
        self.heap_remove(0);
        let handle = TimerHandle {
            queue_id: self.queue_id,
            slot: slot_index,
            generation: slot.generation,
        };
        let overruns = match slot.mode {
            TimerMode::OneShot => {
                self.release_slot(slot_index);
                0
            }
            TimerMode::Periodic { interval } => {
                let periods = now.wrapping_sub(deadline) / interval + 1;
                self.slots[slot_index].deadline =
                    deadline.wrapping_add(interval.wrapping_mul(periods));
                self.slots[slot_index].sequence = self.take_sequence();
                self.heap_insert(slot_index);
                periods - 1
            }
        };
        self.stats.callbacks = self.stats.callbacks.saturating_add(1);
        self.stats.overruns = self.stats.overruns.saturating_add(overruns);
        self.refresh_active_stats();
        Some((
            TimerEvent {
                handle,
                deadline,
                observed_at: now,
                overruns,
            },
            slot.callback,
            slot.context,
        ))
    }

    pub fn next_deadline(&self) -> Option<u64> {
        (self.heap_len != 0).then(|| self.slots[self.heap[0]].deadline)
    }

    pub const fn stats(&self) -> QueueStats {
        self.stats
    }

    fn schedule_error<T>(&mut self, error: Error) -> Result<T, Error> {
        self.stats.schedule_failures = self.stats.schedule_failures.saturating_add(1);
        Err(error)
    }

    fn validate_handle(&self, handle: TimerHandle) -> Result<usize, Error> {
        if handle.queue_id != self.queue_id {
            return Err(Error::InvalidHandle);
        }
        match self.slots.get(handle.slot) {
            Some(slot) if slot.occupied && slot.generation == handle.generation => Ok(handle.slot),
            _ => Err(Error::InvalidHandle),
        }
    }

    fn take_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        sequence
    }

    fn refresh_active_stats(&mut self) {
        self.stats.active_timers = self.heap_len;
        self.stats.peak_timers = self.stats.peak_timers.max(self.heap_len);
    }

    fn release_slot(&mut self, slot_index: usize) {
        self.slots[slot_index].occupied = false;
        self.free_slots[self.free_len] = slot_index;
        self.free_len += 1;
    }

    fn heap_insert(&mut self, slot_index: usize) {
        let heap_index = self.heap_len;
        self.heap[heap_index] = slot_index;
        self.slots[slot_index].heap_index = heap_index;
        self.heap_len += 1;
        indexed_heap::sift_up(&mut HeapView(self), heap_index);
    }

    fn heap_remove(&mut self, heap_index: usize) {
        let removed_slot = self.heap[heap_index];
        self.heap_len -= 1;
        self.slots[removed_slot].heap_index = NONE;
        if heap_index == self.heap_len {
            return;
        }
        let replacement = self.heap[self.heap_len];
        self.heap[heap_index] = replacement;
        self.slots[replacement].heap_index = heap_index;
        indexed_heap::repair(&mut HeapView(self), heap_index);
    }

    fn earlier(&self, left: usize, right: usize) -> bool {
        let left = self.slots[self.heap[left]];
        let right = self.slots[self.heap[right]];
        if left.deadline == right.deadline {
            left.sequence < right.sequence
        } else {
            (left.deadline.wrapping_sub(right.deadline) as i64) < 0
        }
    }

    fn heap_swap(&mut self, left: usize, right: usize) {
        self.heap.swap(left, right);
        self.slots[self.heap[left]].heap_index = left;
        self.slots[self.heap[right]].heap_index = right;
    }
}

impl<const CAPACITY: usize> Default for DeadlineQueue<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) const fn next_generation(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

fn empty_callback(_: TimerEvent, _: usize) {}

struct HeapView<'a, const CAPACITY: usize>(&'a mut DeadlineQueue<CAPACITY>);

impl<const CAPACITY: usize> Storage for HeapView<'_, CAPACITY> {
    fn len(&self) -> usize {
        self.0.heap_len
    }
    fn precedes(&self, left: usize, right: usize) -> bool {
        self.0.earlier(left, right)
    }
    fn swap(&mut self, left: usize, right: usize) {
        self.0.heap_swap(left, right);
    }
}
