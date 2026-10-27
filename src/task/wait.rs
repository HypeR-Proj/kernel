// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduler wait queues for thread-context blocking and IRQ-safe wakeups.

use core::cell::UnsafeCell;

use hyper::cpu::CpuIndex;

use super::scheduler;
use super::thread::ThreadId;

/// Terminal reason selected for one wait registration.
///
/// Notification, timeout, and cancellation arbitrate under the owner CPU lock;
/// exactly one reason can complete a registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Notified,
    TimedOut,
    Cancelled,
}

/// Placement constraint active only while a Thread is queued on a wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitMobility {
    Migratable,
    CpuLocal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedMobility {
    Migratable,
    CpuLocal(CpuIndex),
}

impl QueuedMobility {
    const fn from_request(request: WaitMobility, cpu: CpuIndex) -> Self {
        match request {
            WaitMobility::Migratable => Self::Migratable,
            WaitMobility::CpuLocal => Self::CpuLocal(cpu),
        }
    }

    const fn permits_assignment(self, cpu: CpuIndex) -> bool {
        match self {
            Self::Migratable => true,
            Self::CpuLocal(owner) => owner.get() == cpu.get(),
        }
    }
}

/// Generation-qualified identity of one Thread wait.
///
/// Thread identifiers are never reused and each Thread exhausts rather than
/// wraps its generation. A delayed resolver therefore cannot complete a later
/// wait, even when both waits use the same queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaitTicket {
    thread: ThreadId,
    generation: u64,
}

impl WaitTicket {
    pub(crate) const fn thread(self) -> ThreadId {
        self.thread
    }

    #[cfg(feature = "kernel-trace")]
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitPhase {
    Idle,
    Armed {
        generation: u64,
        mobility: QueuedMobility,
    },
    Queued {
        generation: u64,
        queue: usize,
        mobility: QueuedMobility,
    },
    Completed {
        generation: u64,
        outcome: WaitOutcome,
    },
}

/// Scheduler-owned arbitration record embedded in every Thread.
///
/// The owner CPU lock serializes this record, while queue-link changes also
/// hold the affected queue lock (or exclusive coordination). The record
/// therefore needs neither allocation nor atomics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WaitRecord {
    generation: u64,
    phase: WaitPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaitRecordError {
    GenerationExhausted,
    RegistrationMismatch,
    InvalidPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingResolution {
    Armed,
    Queued { queue: usize },
    AlreadyCompleted,
    Stale,
}

impl WaitRecord {
    pub const NEW: Self = Self {
        generation: 0,
        phase: WaitPhase::Idle,
    };

    pub fn arm(
        &mut self,
        thread: ThreadId,
        mobility: WaitMobility,
        cpu: CpuIndex,
    ) -> Result<WaitTicket, WaitRecordError> {
        if self.phase != WaitPhase::Idle {
            return Err(WaitRecordError::InvalidPhase);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(WaitRecordError::GenerationExhausted)?;
        self.phase = WaitPhase::Armed {
            generation: self.generation,
            mobility: QueuedMobility::from_request(mobility, cpu),
        };
        Ok(WaitTicket {
            thread,
            generation: self.generation,
        })
    }

    pub fn queue(&mut self, ticket: WaitTicket, queue: usize) -> Result<(), WaitRecordError> {
        self.require_ticket(ticket)?;
        let WaitPhase::Armed {
            generation,
            mobility,
        } = self.phase
        else {
            return Err(WaitRecordError::InvalidPhase);
        };
        if generation != ticket.generation {
            return Err(WaitRecordError::RegistrationMismatch);
        }
        self.phase = WaitPhase::Queued {
            generation: ticket.generation,
            queue,
            mobility,
        };
        crate::tracepoint!(
            Scheduler,
            "wait_block",
            thread = ticket.thread().get(),
            wait = ticket.generation()
        );
        Ok(())
    }

    pub fn pending_resolution(
        &self,
        ticket: WaitTicket,
    ) -> Result<PendingResolution, WaitRecordError> {
        if ticket.generation != self.generation {
            return Ok(PendingResolution::Stale);
        }
        match self.phase {
            WaitPhase::Idle => Ok(PendingResolution::Stale),
            WaitPhase::Armed { generation, .. } if generation == ticket.generation => {
                Ok(PendingResolution::Armed)
            }
            WaitPhase::Queued {
                generation, queue, ..
            } if generation == ticket.generation => Ok(PendingResolution::Queued { queue }),
            WaitPhase::Completed { generation, .. } if generation == ticket.generation => {
                Ok(PendingResolution::AlreadyCompleted)
            }
            _ => Err(WaitRecordError::RegistrationMismatch),
        }
    }

    pub fn queued_ticket(&self, thread: ThreadId, queue: usize) -> Option<WaitTicket> {
        match self.phase {
            WaitPhase::Queued {
                generation,
                queue: member_queue,
                ..
            } if member_queue == queue => Some(WaitTicket { thread, generation }),
            _ => None,
        }
    }

    pub fn complete(
        &mut self,
        ticket: WaitTicket,
        outcome: WaitOutcome,
    ) -> Result<(), WaitRecordError> {
        self.require_ticket(ticket)?;
        if !matches!(
            self.phase,
            WaitPhase::Armed { generation, .. } | WaitPhase::Queued { generation, .. }
                if generation == ticket.generation
        ) {
            return Err(WaitRecordError::InvalidPhase);
        }
        self.phase = WaitPhase::Completed {
            generation: ticket.generation,
            outcome,
        };
        crate::tracepoint!(
            Scheduler,
            "wait_complete",
            thread = ticket.thread().get(),
            wait = ticket.generation(),
            outcome = match outcome {
                WaitOutcome::Notified => 1,
                WaitOutcome::TimedOut => 2,
                WaitOutcome::Cancelled => 3,
            },
            waker = crate::diagnostics::trace::actor()
        );
        Ok(())
    }

    /// Consumes an unqueued registration or its already-selected outcome.
    pub fn finish_unqueued(
        &mut self,
        ticket: WaitTicket,
    ) -> Result<Option<WaitOutcome>, WaitRecordError> {
        self.require_ticket(ticket)?;
        let outcome = match self.phase {
            WaitPhase::Armed { generation, .. } if generation == ticket.generation => None,
            WaitPhase::Completed {
                generation,
                outcome,
            } if generation == ticket.generation => Some(outcome),
            _ => return Err(WaitRecordError::InvalidPhase),
        };
        self.phase = WaitPhase::Idle;
        Ok(outcome)
    }

    pub fn finish_completed(&mut self, ticket: WaitTicket) -> Result<WaitOutcome, WaitRecordError> {
        self.require_ticket(ticket)?;
        let WaitPhase::Completed {
            generation,
            outcome,
        } = self.phase
        else {
            return Err(WaitRecordError::InvalidPhase);
        };
        if generation != ticket.generation {
            return Err(WaitRecordError::RegistrationMismatch);
        }
        self.phase = WaitPhase::Idle;
        Ok(outcome)
    }

    pub const fn permits_assignment(&self, cpu: CpuIndex) -> bool {
        match self.phase {
            WaitPhase::Armed { mobility, .. } | WaitPhase::Queued { mobility, .. } => {
                mobility.permits_assignment(cpu)
            }
            _ => true,
        }
    }

    pub const fn is_idle(&self) -> bool {
        matches!(self.phase, WaitPhase::Idle)
    }

    pub(super) fn current_ticket(&self, thread: ThreadId) -> Option<WaitTicket> {
        match self.phase {
            WaitPhase::Queued { generation, .. }
            | WaitPhase::Armed { generation, .. }
            | WaitPhase::Completed { generation, .. } => Some(WaitTicket { thread, generation }),
            WaitPhase::Idle => None,
        }
    }

    fn require_ticket(&self, ticket: WaitTicket) -> Result<(), WaitRecordError> {
        if ticket.generation == self.generation {
            Ok(())
        } else {
            Err(WaitRecordError::RegistrationMismatch)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ThreadQueue {
    pub head: Option<ThreadId>,
    pub tail: Option<ThreadId>,
    pub len: usize,
}

impl ThreadQueue {
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }
}

/// FIFO queue of blocked threads.
///
/// Queue links live inside each scheduler-owned `Thread`, so waiting and
/// waking never allocate. The queue must outlive every thread waiting on it;
/// safe users normally satisfy this by embedding it in a static object or in
/// an object retained by the blocked thread's stack/owner.
pub struct WaitQueue {
    state: UnsafeCell<ThreadQueue>,
    lock: hyper::sync::InterruptSpinLock<(), crate::hal::irq::LocalMask>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(ThreadQueue::new()),
            lock: hyper::sync::InterruptSpinLock::new(()),
        }
    }

    /// Blocks the current thread until a matching wake operation dequeues it.
    ///
    /// A wait queue does not retain wakeups, so this is an unconditional park,
    /// not an event counter. Condition-based primitives should combine their
    /// state lock with the scheduler's locked-park path as Mutex/Semaphore do.
    pub fn wait(&self) -> Result<WaitOutcome, scheduler::Error> {
        let token = scheduler::prepare_park(self)?;
        Ok(scheduler::complete_park(token))
    }

    pub fn wake_one(&self) -> Result<Option<ThreadId>, scheduler::Error> {
        scheduler::wake_one(self)
    }

    pub fn wake_all(&self) -> Result<usize, scheduler::Error> {
        scheduler::wake_all(self)
    }

    /// Cancels `thread` only when it is currently queued on this wait queue.
    ///
    /// Missing Threads, Armed registrations, and waits on another queue return
    /// `false`. A winning cancellation removes the exact current generation,
    /// publishes the Thread as runnable, and causes its wait to return
    /// [`WaitOutcome::Cancelled`]. The operation is allocation-free and may be
    /// called from IRQ context.
    pub fn cancel(&self, thread: ThreadId) -> Result<bool, scheduler::Error> {
        scheduler::cancel_waiter(self, thread)
    }

    pub fn len(&self) -> Result<usize, scheduler::Error> {
        scheduler::waiter_count(self)
    }

    pub fn is_empty(&self) -> Result<bool, scheduler::Error> {
        self.len().map(|len| len == 0)
    }

    pub(super) fn identity(&self) -> usize {
        core::ptr::from_ref(self).expose_provenance()
    }

    /// Returns the internal pointer accessed only under exclusive coordination.
    pub(super) const fn state_pointer(&self) -> *mut ThreadQueue {
        self.state.get()
    }

    /// Accesses FIFO topology under shared registry protection. The exclusive
    /// coordinator excludes all such readers and can access state directly.
    ///
    /// # Safety
    ///
    /// The caller must retain shared or exclusive registry protection for the
    /// entire closure, excluding direct coordinator access to this cell.
    pub(super) unsafe fn with_state<R>(&self, operation: impl FnOnce(&mut ThreadQueue) -> R) -> R {
        self.lock.with(|()| {
            // SAFETY: callers hold a registry reader lane, excluding direct
            // coordinator access; this queue's lock excludes other readers.
            operation(unsafe { &mut *self.state.get() })
        })
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: accesses either hold the exclusive coordinator or a registry reader
// lane plus this queue's lock. These two access modes exclude each other.
unsafe impl Sync for WaitQueue {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(index: usize) -> CpuIndex {
        match CpuIndex::new(index) {
            Some(cpu) => cpu,
            None => panic!("test CPU index is invalid"),
        }
    }

    #[test]
    fn exact_ticket_selects_only_one_outcome() {
        let thread = ThreadId::for_test(7);
        let mut wait = WaitRecord::NEW;
        let ticket = match wait.arm(thread, WaitMobility::Migratable, cpu(0)) {
            Ok(ticket) => ticket,
            Err(error) => panic!("failed to arm wait: {error:?}"),
        };
        assert_eq!(
            wait.pending_resolution(ticket),
            Ok(PendingResolution::Armed)
        );
        assert_eq!(wait.complete(ticket, WaitOutcome::TimedOut), Ok(()));
        assert_eq!(
            wait.pending_resolution(ticket),
            Ok(PendingResolution::AlreadyCompleted)
        );
        assert_eq!(
            wait.finish_unqueued(ticket),
            Ok(Some(WaitOutcome::TimedOut))
        );
    }

    #[test]
    fn stale_ticket_cannot_resolve_reused_queue() {
        let thread = ThreadId::for_test(11);
        let mut wait = WaitRecord::NEW;
        let stale = match wait.arm(thread, WaitMobility::Migratable, cpu(0)) {
            Ok(ticket) => ticket,
            Err(error) => panic!("failed to arm first wait: {error:?}"),
        };
        assert_eq!(wait.finish_unqueued(stale), Ok(None));
        let current = match wait.arm(thread, WaitMobility::Migratable, cpu(0)) {
            Ok(ticket) => ticket,
            Err(error) => panic!("failed to arm second wait: {error:?}"),
        };
        assert_eq!(wait.pending_resolution(stale), Ok(PendingResolution::Stale));
        assert_eq!(
            wait.pending_resolution(current),
            Ok(PendingResolution::Armed)
        );
    }

    #[test]
    fn cpu_local_constraint_applies_while_armed_and_queued() {
        let thread = ThreadId::for_test(19);
        let mut wait = WaitRecord::NEW;
        let ticket = match wait.arm(thread, WaitMobility::CpuLocal, cpu(0)) {
            Ok(ticket) => ticket,
            Err(error) => panic!("failed to arm CPU-local wait: {error:?}"),
        };
        assert!(wait.permits_assignment(cpu(0)));
        assert!(!wait.permits_assignment(cpu(1)));
        assert_eq!(wait.queue(ticket, 0x1000), Ok(()));
        assert!(wait.permits_assignment(cpu(0)));
        assert!(!wait.permits_assignment(cpu(1)));
    }
}
