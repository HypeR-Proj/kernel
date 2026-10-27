// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Preallocated timer nodes in an augmented, owned red-black tree.
//!
//! Immutable identity orders the tree, permitting stale-safe logarithmic
//! cancellation without a second index. Each subtree caches its earliest
//! deadline. Allocation and destruction stay outside queue mutation.

#![forbid(unsafe_code)]

use alloc::boxed::Box;

use super::fixed::{Error, QueueStats, TimerCallback, TimerEvent, TimerHandle, TimerMode};

mod tree;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeadlineKey {
    time: u128,
    sequence: u64,
    identity: u64,
}

struct TimerNode {
    identity: u64,
    deadline: u64,
    sequence: u64,
    mode: TimerMode,
    callback: TimerCallback,
    context: usize,
    claim: Option<TimerClaim>,
    claim_context: usize,
    recycle: Option<TimerRecycle>,
    recycle_context: usize,
    time: u128,
    earliest: DeadlineKey,
    red: bool,
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

pub type TimerRecycle = fn(ReservedTimerNode, usize);
pub type TimerClaim = fn(TimerEvent, usize);

pub struct ReservedTimerCallbacks {
    pub callback: TimerCallback,
    pub context: usize,
    pub claim: TimerClaim,
    pub claim_context: usize,
    pub recycle: TimerRecycle,
    pub recycle_context: usize,
}

/// An allocated timer node that is not linked into a queue.
///
/// Construct this before acquiring a queue lock and drop cancelled timers
/// after releasing it. This keeps the global allocator outside the timer
/// queue's lock ordering.
pub struct PendingTimer(Box<TimerNode>);

impl PendingTimer {
    pub const fn allocation_size() -> usize {
        core::mem::size_of::<TimerNode>()
    }

    pub fn try_new(
        deadline: u64,
        mode: TimerMode,
        callback: TimerCallback,
        context: usize,
    ) -> Result<Self, Error> {
        if matches!(
            mode,
            TimerMode::Periodic {
                interval: 0 | 0x8000_0000_0000_0000..
            }
        ) {
            return Err(Error::InvalidInterval);
        }
        hyper::mm::try_box(TimerNode {
            identity: 0,
            deadline,
            sequence: 0,
            mode,
            callback,
            context,
            claim: None,
            claim_context: 0,
            recycle: None,
            recycle_context: 0,
            time: 0,
            earliest: DeadlineKey {
                time: 0,
                sequence: 0,
                identity: 0,
            },
            red: true,
            left: None,
            right: None,
        })
        .map(Self)
        .map_err(|_| Error::Allocation)
    }
}

/// One preallocated one-shot node reserved for allocation-free rearming.
pub struct ReservedTimerNode(Box<TimerNode>);

impl ReservedTimerNode {
    pub const fn allocation_size() -> usize {
        core::mem::size_of::<TimerNode>()
    }

    pub fn try_new() -> Result<Self, Error> {
        PendingTimer::try_new(0, TimerMode::OneShot, reserved_unarmed, 0)
            .map(|pending| Self(pending.0))
    }

    pub fn prepare(
        mut self,
        deadline: u64,
        callbacks: ReservedTimerCallbacks,
    ) -> PendingReservedTimer {
        self.0.deadline = deadline;
        self.0.mode = TimerMode::OneShot;
        self.0.callback = callbacks.callback;
        self.0.context = callbacks.context;
        self.0.claim = Some(callbacks.claim);
        self.0.claim_context = callbacks.claim_context;
        self.0.recycle = Some(callbacks.recycle);
        self.0.recycle_context = callbacks.recycle_context;
        PendingReservedTimer(self.0)
    }
}

/// Configured reserved node not yet linked into a deadline queue.
pub struct PendingReservedTimer(Box<TimerNode>);

/// A callback delivery whose one-shot node, if any, remains owned until the
/// delivery is invoked or dropped outside the queue lock.
#[must_use = "expired timers must be invoked; reserved deliveries recover on drop"]
pub struct ExpiredTimer {
    event: TimerEvent,
    callback: TimerCallback,
    context: usize,
    retired: Option<RetiredTimer>,
}

enum RetiredTimer {
    Ordinary(PendingTimer),
    Reserved(ReservedTimerNode, TimerRecycle, usize),
}

impl ExpiredTimer {
    pub const fn event(&self) -> TimerEvent {
        self.event
    }

    pub const fn context(&self) -> usize {
        self.context
    }

    pub fn invoke(mut self) {
        (self.callback)(self.event, self.context);
        self.recycle();
    }

    fn recycle(&mut self) {
        match self.retired.take() {
            Some(RetiredTimer::Ordinary(timer)) => drop(timer),
            Some(RetiredTimer::Reserved(node, recycle, recycle_context)) => {
                recycle(node, recycle_context)
            }
            None => {}
        }
    }
}

impl Drop for ExpiredTimer {
    fn drop(&mut self) {
        if matches!(self.retired, Some(RetiredTimer::Reserved(..))) {
            // A reserved expiry was already claimed under its queue lock. Its
            // notification is therefore mandatory even when a safe caller
            // abandons the delivery value.
            (self.callback)(self.event, self.context);
        }
        self.recycle();
    }
}

/// A cached-minimum red-black tree of independently owned timer allocations.
///
/// Insert, cancel, reset and expiry cost O(log n); the next deadline is O(1).
/// Identity orders the tree, while subtree augmentation selects the earliest
/// deadline. An old handle is looked up by value and never dereferenced.
///
/// `now` must be sampled while holding the caller's queue lock. Successive
/// samples must advance by less than half the hardware counter range. A raw
/// deadline is interpreted as a signed counter offset from that sample. The
/// queue extends counter time before comparing deadlines, so an overdue timer
/// and a timer nearly half a counter range in the future remain ordered even
/// when their raw timestamps span more than half the counter range.
///
/// Every node is prepared outside the lock. Removed nodes are returned to the
/// caller for destruction or recycling outside it; balancing never allocates.
pub struct OwnedDeadlineQueue {
    queue_id: usize,
    root: Option<Box<TimerNode>>,
    len: usize,
    next_identity: u64,
    next_sequence: u64,
    last_counter: Option<u64>,
    time: u128,
    stats: QueueStats,
}

impl OwnedDeadlineQueue {
    pub const fn new() -> Self {
        Self {
            queue_id: 0,
            root: None,
            len: 0,
            next_identity: 0,
            next_sequence: 0,
            last_counter: None,
            // Leave one complete counter era before the first observation,
            // allowing already-expired deadlines without unsigned underflow.
            time: 1_u128 << 64,
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

    pub fn initialize_id(&mut self, queue_id: usize) -> Result<(), Error> {
        if self.stats.schedules != 0 || self.len != 0 {
            return Err(Error::QueueAlreadyUsed);
        }
        self.queue_id = queue_id;
        Ok(())
    }

    /// Links a preallocated timer without invoking the allocator.
    pub fn insert(&mut self, pending: PendingTimer, now: u64) -> TimerHandle {
        self.insert_new(pending.0, now)
    }

    /// Links a preallocated reserved node without invoking the allocator.
    pub fn insert_reserved(&mut self, pending: PendingReservedTimer, now: u64) -> TimerHandle {
        self.insert_new(pending.0, now)
    }

    fn insert_new(&mut self, mut node: Box<TimerNode>, now: u64) -> TimerHandle {
        self.observe(now);
        let identity = self.take_identity();
        node.identity = identity;
        node.sequence = self.take_sequence();
        node.time = self.deadline_time(node.deadline, now);
        self.insert_node(node);
        self.stats.schedules = self.stats.schedules.saturating_add(1);
        self.refresh_active_stats();
        TimerHandle::owned(self.queue_id, identity)
    }

    /// Unlinks a timer and returns its allocation for destruction outside the
    /// caller's lock. Reserved nodes require `cancel_reserved` instead.
    pub fn cancel(&mut self, handle: TimerHandle) -> Result<PendingTimer, Error> {
        let node = self.detach(handle, false)?;
        self.stats.cancellations = self.stats.cancellations.saturating_add(1);
        self.refresh_active_stats();
        Ok(PendingTimer(node))
    }

    pub fn cancel_reserved(&mut self, handle: TimerHandle) -> Result<ReservedTimerNode, Error> {
        let mut node = self.detach(handle, true)?;
        node.claim = None;
        node.claim_context = 0;
        node.recycle = None;
        node.recycle_context = 0;
        self.stats.cancellations = self.stats.cancellations.saturating_add(1);
        self.refresh_active_stats();
        Ok(ReservedTimerNode(node))
    }

    pub fn reschedule(
        &mut self,
        handle: TimerHandle,
        deadline: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.reset(handle, deadline, now, false)
    }

    /// Updates an armed reservation in place, retaining its callback owner and
    /// handle identity. The caller serializes its arm generation with this
    /// mutation under the same lock used by the expiry claim.
    pub fn reschedule_reserved(
        &mut self,
        handle: TimerHandle,
        deadline: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.reset(handle, deadline, now, true)
    }

    fn reset(
        &mut self,
        handle: TimerHandle,
        deadline: u64,
        now: u64,
        reserved: bool,
    ) -> Result<(), Error> {
        let identity = self.validate_handle(handle, reserved)?;
        self.observe(now);
        let time = self.deadline_time(deadline, now);
        let sequence = self.take_sequence();
        tree::reset(&mut self.root, identity, deadline, time, sequence);
        self.stats.reschedules = self.stats.reschedules.saturating_add(1);
        Ok(())
    }

    pub fn pop_expired(&mut self, now: u64) -> Option<ExpiredTimer> {
        self.observe(now);
        let first = self.root.as_ref()?.earliest;
        if first.time > self.time {
            return None;
        }
        let mut node = self.remove_node(first.identity);
        let deadline = node.deadline;
        let identity = node.identity;
        let mode = node.mode;
        let callback = node.callback;
        let context = node.context;
        let event = TimerEvent {
            handle: TimerHandle::owned(self.queue_id, identity),
            deadline,
            observed_at: now,
            overruns: 0,
        };
        if let Some(claim) = node.claim.take() {
            claim(event, node.claim_context);
        }
        let (overruns, retired) = match mode {
            TimerMode::OneShot => {
                let retired = match (node.recycle.take(), node.recycle_context) {
                    (Some(recycle), recycle_context) => {
                        RetiredTimer::Reserved(ReservedTimerNode(node), recycle, recycle_context)
                    }
                    (None, _) => RetiredTimer::Ordinary(PendingTimer(node)),
                };
                (0, Some(retired))
            }
            TimerMode::Periodic { interval } => {
                // Work in extended time: an overdue backlog can be older than
                // half a raw counter era even though each observation is close.
                let elapsed = self.time - node.time;
                let periods = if elapsed <= u128::from(u64::MAX) {
                    // Hardware division handles the ordinary one-counter-era
                    // case; extended division is only needed for ancient work.
                    u128::from(elapsed as u64 / interval) + 1
                } else {
                    elapsed / u128::from(interval) + 1
                };
                let advance = match u128::from(interval).checked_mul(periods) {
                    Some(advance) => advance,
                    None => queue_invariant_failure(),
                };
                node.time = match node.time.checked_add(advance) {
                    Some(time) => time,
                    None => queue_invariant_failure(),
                };
                node.deadline = deadline.wrapping_add(advance as u64);
                node.sequence = self.take_sequence();
                self.insert_node(node);
                ((periods - 1).min(u128::from(u64::MAX)) as u64, None)
            }
        };
        self.stats.callbacks = self.stats.callbacks.saturating_add(1);
        self.stats.overruns = self.stats.overruns.saturating_add(overruns);
        self.refresh_active_stats();
        Some(ExpiredTimer {
            event: TimerEvent { overruns, ..event },
            callback,
            context,
            retired,
        })
    }

    pub fn next_deadline(&self) -> Option<u64> {
        let first = self.root.as_ref()?.earliest;
        // Convert the cached extended timestamp back to a hardware counter
        // without searching the identity tree.
        let now = self.last_counter?;
        Some(now.wrapping_add(first.time.wrapping_sub(self.time) as u64))
    }

    /// Returns a deadline suitable for the wrapping hardware comparator.
    ///
    /// An ancient overdue timestamp can look future to a signed hardware
    /// comparison. Clamp due work to the latest observation instead; delivery
    /// still reports the original deadline through `TimerEvent`.
    pub fn next_programming_deadline(&self) -> Option<u64> {
        if self.root.as_ref()?.earliest.time <= self.time {
            self.last_counter
        } else {
            self.next_deadline()
        }
    }

    pub const fn stats(&self) -> QueueStats {
        self.stats
    }

    /// Checks ordering, red-black balance, cached minima and node count.
    ///
    /// This diagnostic is allocation-free and O(n); it does not belong on a
    /// timer interrupt or ordinary queue mutation path.
    pub fn validate_tree(&self) -> bool {
        tree::validate(&self.root, self.len)
    }

    fn validate_handle(&self, handle: TimerHandle, reserved: bool) -> Result<u64, Error> {
        let identity = handle
            .owned_identity(self.queue_id)
            .ok_or(Error::InvalidHandle)?;
        let node = tree::find(&self.root, identity).ok_or(Error::InvalidHandle)?;
        if node.recycle.is_some() != reserved {
            return Err(Error::InvalidHandle);
        }
        Ok(identity)
    }

    fn detach(&mut self, handle: TimerHandle, reserved: bool) -> Result<Box<TimerNode>, Error> {
        let identity = self.validate_handle(handle, reserved)?;
        Ok(self.remove_node(identity))
    }

    fn remove_node(&mut self, identity: u64) -> Box<TimerNode> {
        let (root, node) = tree::remove(self.root.take(), identity);
        self.root = root;
        self.len -= 1;
        node
    }

    fn insert_node(&mut self, node: Box<TimerNode>) {
        self.root = Some(tree::insert(self.root.take(), node));
        self.len += 1;
    }

    fn observe(&mut self, now: u64) {
        if let Some(previous) = self.last_counter {
            let delta = now.wrapping_sub(previous);
            if delta > i64::MAX as u64 {
                // Samples are taken under the source queue lock. Going
                // backwards proves a caller violated its clock contract.
                queue_invariant_failure();
            }
            self.time = match self.time.checked_add(u128::from(delta)) {
                Some(time) => time,
                None => queue_invariant_failure(),
            };
        }
        self.last_counter = Some(now);
    }

    fn deadline_time(&self, deadline: u64, now: u64) -> u128 {
        let offset = deadline.wrapping_sub(now) as i64;
        if offset < 0 {
            self.time - u128::from(offset.unsigned_abs())
        } else {
            match self.time.checked_add(offset as u128) {
                Some(time) => time,
                None => queue_invariant_failure(),
            }
        }
    }

    fn take_identity(&mut self) -> u64 {
        self.next_identity = next_counter(self.next_identity);
        self.next_identity
    }

    fn take_sequence(&mut self) -> u64 {
        self.next_sequence = next_counter(self.next_sequence);
        self.next_sequence
    }

    fn refresh_active_stats(&mut self) {
        self.stats.active_timers = self.len;
        self.stats.peak_timers = self.stats.peak_timers.max(self.len);
    }
}

fn next_counter(current: u64) -> u64 {
    match current.checked_add(1) {
        Some(next) => next,
        // Reusing an identity would let an ancient handle cancel a new timer;
        // reusing a sequence would silently break equal-deadline FIFO order.
        None => queue_invariant_failure(),
    }
}

fn reserved_unarmed(_event: TimerEvent, _context: usize) {}

impl Default for OwnedDeadlineQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn queue_invariant_failure() -> ! {
    hyper::debug::invariant_failure("owned timer queue ordering or identity invariant")
}
