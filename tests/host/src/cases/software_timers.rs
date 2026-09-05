// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Software-timer ordering, identity, cancellation, and periodic semantics.

use hyper::time::{
    DeadlineQueue, OwnedDeadlineQueue, PendingTimer, ReservedTimerCallbacks, ReservedTimerNode,
    TimerEvent, TimerMode, TimerQueueError,
};

struct ReservedProbe {
    node: std::sync::Mutex<Option<ReservedTimerNode>>,
    claims: std::sync::atomic::AtomicUsize,
    callbacks: std::sync::atomic::AtomicUsize,
}

fn probe(context: usize) -> &'static ReservedProbe {
    let pointer = core::ptr::with_exposed_provenance::<ReservedProbe>(context);
    // SAFETY: the test retains its boxed probe until every queued callback and
    // recycled node has completed.
    unsafe { &*pointer }
}

fn reserved_claim(_event: TimerEvent, context: usize) {
    probe(context)
        .claims
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn reserved_callback(_event: TimerEvent, context: usize) {
    probe(context)
        .callbacks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn reserved_recycle(node: ReservedTimerNode, context: usize) {
    let mut slot = crate::require_ok(probe(context).node.lock());
    assert!(slot.replace(node).is_none());
}

fn callback(_: TimerEvent, _: usize) {}

#[test]
fn orders_cancels_and_reschedules_deadlines() {
    let mut queue = DeadlineQueue::<4>::new();
    let late = crate::require_ok(queue.schedule(30, TimerMode::OneShot, callback, 30));
    let early = crate::require_ok(queue.schedule(10, TimerMode::OneShot, callback, 10));
    let cancelled = crate::require_ok(queue.schedule(20, TimerMode::OneShot, callback, 20));
    crate::require_ok(queue.reschedule(late, 5));
    crate::require_ok(queue.cancel(cancelled));

    assert!(queue.pop_expired(4).is_none());
    let (event, _, context) = crate::require_some(queue.pop_expired(5));
    assert_eq!(event.handle, late);
    assert_eq!(context, 30);
    assert_eq!(queue.cancel(late), Err(TimerQueueError::InvalidHandle));
    let (event, _, context) = crate::require_some(queue.pop_expired(10));
    assert_eq!(event.handle, early);
    assert_eq!(context, 10);
    assert_eq!(queue.next_deadline(), None);

    let stats = queue.stats();
    assert_eq!(stats.peak_timers, 3);
    assert_eq!(stats.cancellations, 1);
    assert_eq!(stats.reschedules, 1);
    assert_eq!(stats.callbacks, 2);
}

#[test]
fn periodic_timer_preserves_phase_and_reports_overruns() {
    let mut queue = DeadlineQueue::<2>::new();
    let periodic =
        crate::require_ok(queue.schedule(100, TimerMode::Periodic { interval: 10 }, callback, 7));

    let (event, _, context) = crate::require_some(queue.pop_expired(135));
    assert_eq!(event.handle, periodic);
    assert_eq!(event.deadline, 100);
    assert_eq!(event.overruns, 3);
    assert_eq!(context, 7);
    assert_eq!(queue.next_deadline(), Some(140));
    crate::require_ok(queue.cancel(periodic));
    assert_eq!(queue.stats().overruns, 3);
}

#[test]
fn preserves_fifo_order_for_equal_deadlines() {
    let mut queue = DeadlineQueue::<3>::new();
    for context in 1..=3 {
        crate::require_ok(queue.schedule(10, TimerMode::OneShot, callback, context));
    }
    for expected in 1..=3 {
        let (_, _, context) = crate::require_some(queue.pop_expired(10));
        assert_eq!(context, expected);
    }
}

#[test]
fn rejects_invalid_periodic_intervals() {
    let mut queue = DeadlineQueue::<2>::new();
    assert_eq!(
        queue.schedule(10, TimerMode::Periodic { interval: 0 }, callback, 0,),
        Err(TimerQueueError::InvalidInterval)
    );
    assert_eq!(
        queue.schedule(
            10,
            TimerMode::Periodic {
                interval: 1u64 << 63,
            },
            callback,
            0,
        ),
        Err(TimerQueueError::InvalidInterval)
    );
    assert_eq!(queue.stats().schedule_failures, 2);
    assert_eq!(queue.stats().active_timers, 0);
}

#[test]
fn rejects_stale_handles_and_capacity_overflow() {
    let mut queue = DeadlineQueue::<1>::new();
    let old = crate::require_ok(queue.schedule(1, TimerMode::OneShot, callback, 0));
    assert_eq!(
        queue.schedule(2, TimerMode::OneShot, callback, 0),
        Err(TimerQueueError::Full)
    );
    crate::require_ok(queue.cancel(old));
    let replacement = crate::require_ok(queue.schedule(3, TimerMode::OneShot, callback, 0));
    assert_ne!(replacement, old);
    assert_eq!(queue.cancel(old), Err(TimerQueueError::InvalidHandle));

    let mut other_queue = DeadlineQueue::<1>::with_id(1);
    assert_eq!(
        other_queue.cancel(replacement),
        Err(TimerQueueError::InvalidHandle)
    );
}

#[test]
fn binds_a_static_queue_identity_only_before_first_use() {
    let mut queue = DeadlineQueue::<1>::new();
    crate::require_ok(queue.initialize_id(7));
    let handle = crate::require_ok(queue.schedule(1, TimerMode::OneShot, callback, 0));
    assert_eq!(
        queue.initialize_id(8),
        Err(TimerQueueError::QueueAlreadyUsed)
    );

    let mut other_queue = DeadlineQueue::<1>::new();
    assert_eq!(
        other_queue.cancel(handle),
        Err(TimerQueueError::InvalidHandle)
    );
}

#[test]
fn compares_deadlines_across_counter_wraparound() {
    let mut queue = DeadlineQueue::<2>::new();
    let before_wrap =
        crate::require_ok(queue.schedule(u64::MAX - 2, TimerMode::OneShot, callback, 1));
    let after_wrap = crate::require_ok(queue.schedule(3, TimerMode::OneShot, callback, 2));

    assert_eq!(
        crate::require_some(queue.pop_expired(u64::MAX - 1))
            .0
            .handle,
        before_wrap
    );
    assert_eq!(
        crate::require_some(queue.pop_expired(3)).0.handle,
        after_wrap
    );
}

#[test]
fn owned_queue_grows_beyond_the_former_kernel_slot_limit() {
    let mut queue = OwnedDeadlineQueue::new();
    crate::require_ok(queue.initialize_id(3));
    for context in 0..600 {
        let pending = crate::require_ok(PendingTimer::try_new(
            10,
            TimerMode::OneShot,
            callback,
            context,
        ));
        let _handle = queue.insert(pending);
    }
    assert_eq!(queue.stats().active_timers, 600);
    assert_eq!(queue.stats().peak_timers, 600);

    for expected in 0..600 {
        let expired = crate::require_some(queue.pop_expired(10));
        assert_eq!(expired.context(), expected);
        assert_eq!(expired.event().deadline, 10);
    }
    assert_eq!(queue.next_deadline(), None);
}

#[test]
fn owned_queue_rejects_stale_and_foreign_handles() {
    let mut queue = OwnedDeadlineQueue::new();
    let old = queue.insert(crate::require_ok(PendingTimer::try_new(
        1,
        TimerMode::OneShot,
        callback,
        0,
    )));
    let retired = crate::require_ok(queue.cancel(old));
    drop(retired);
    let replacement = queue.insert(crate::require_ok(PendingTimer::try_new(
        2,
        TimerMode::OneShot,
        callback,
        0,
    )));
    assert_ne!(old, replacement);
    assert!(matches!(
        queue.cancel(old),
        Err(TimerQueueError::InvalidHandle)
    ));

    let mut other = OwnedDeadlineQueue::new();
    crate::require_ok(other.initialize_id(1));
    assert!(matches!(
        other.cancel(replacement),
        Err(TimerQueueError::InvalidHandle)
    ));
}

#[test]
fn reserved_owned_timer_recycles_without_entering_ordinary_cancellation() {
    let probe = Box::leak(Box::new(ReservedProbe {
        node: std::sync::Mutex::new(Some(crate::require_ok(ReservedTimerNode::try_new()))),
        claims: std::sync::atomic::AtomicUsize::new(0),
        callbacks: std::sync::atomic::AtomicUsize::new(0),
    }));
    let context = core::ptr::from_ref(&*probe).expose_provenance();
    let callbacks = || ReservedTimerCallbacks {
        callback: reserved_callback,
        context,
        claim: reserved_claim,
        claim_context: context,
        recycle: reserved_recycle,
        recycle_context: context,
    };
    let mut queue = OwnedDeadlineQueue::new();
    let first_node = crate::require_some(crate::require_ok(probe.node.lock()).take());
    let first = queue.insert_reserved(first_node.prepare(10, callbacks()));
    assert!(matches!(
        queue.cancel(first),
        Err(TimerQueueError::InvalidHandle)
    ));
    crate::require_some(queue.pop_expired(10)).invoke();
    assert_eq!(probe.claims.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        probe.callbacks.load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let recycled = crate::require_some(crate::require_ok(probe.node.lock()).take());
    let second = queue.insert_reserved(recycled.prepare(20, callbacks()));
    let recovered = crate::require_ok(queue.cancel_reserved(second));
    assert!(
        crate::require_ok(probe.node.lock())
            .replace(recovered)
            .is_none()
    );

    let recovered = crate::require_some(crate::require_ok(probe.node.lock()).take());
    let _third = queue.insert_reserved(recovered.prepare(30, callbacks()));
    drop(crate::require_some(queue.pop_expired(30)));
    assert_eq!(probe.claims.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(
        probe.callbacks.load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert!(crate::require_ok(probe.node.lock()).is_some());
}

fn source_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = crate::require_some(source.find(start));
    let end = crate::require_some(source[start..].find(end)) + start;
    &source[start..end]
}

fn assert_source_order(region: &str, markers: &[&str]) {
    let mut previous = 0;
    for marker in markers {
        let position = crate::require_some(region.find(marker));
        assert!(position >= previous, "{marker} appears out of order");
        previous = position;
    }
}

#[test]
fn cpu_local_timer_transactions_remain_pinned_through_comparator_programming() {
    let source = include_str!("../../../../src/kernel/time/timers.rs");

    let arm = source_region(
        source,
        "    pub(crate) fn arm(",
        "    fn arm_on_pinned_cpu(",
    );
    assert_source_order(
        arm,
        &[
            "pin_current_timer_cpu()?",
            "self.arm_on_pinned_cpu(",
            "release_timer_cpu_pin(cpu_pin)",
        ],
    );
    let arm_transaction = source_region(
        source,
        "    fn arm_on_pinned_cpu(",
        "    fn restore_cancelled(",
    );
    assert!(arm_transaction.contains("program_next(&timers.queue)"));

    let schedule = source_region(source, "pub fn schedule_at(", "pub fn schedule_after(");
    let allocation = crate::require_some(schedule.find("PendingTimer::try_new("));
    let pin = crate::require_some(schedule.find("pin_current_timer_cpu()?"));
    let insert = crate::require_some(schedule.find("timers.queue.insert(pending)"));
    let program = crate::require_some(schedule.find("program_next(&timers.queue)"));
    let release = crate::require_some(schedule.rfind("release_timer_cpu_pin(cpu_pin)"));
    let retire = crate::require_some(schedule.find("drop(retired)"));
    assert!(allocation < pin);
    assert!(pin < insert);
    assert!(insert < program);
    assert!(program < release);
    assert!(release < retire);

    let cancel = source_region(source, "pub fn cancel(", "pub fn local_statistics(");
    assert_source_order(
        cancel,
        &[
            "pin_current_timer_cpu()?",
            "TIMERS[owner].with(",
            "program_next(&timers.queue)",
            "release_timer_cpu_pin(cpu_pin)",
            "drop(retired)",
        ],
    );

    let statistics = source_region(
        source,
        "pub fn local_statistics(",
        "pub(super) fn handle_interrupt(",
    );
    assert_source_order(
        statistics,
        &[
            "pin_current_timer_cpu().ok()?",
            "TIMERS[cpu].with(",
            "release_timer_cpu_pin(cpu_pin)",
        ],
    );

    let retire = source_region(source, "    fn retire(", "}\n\nimpl Drop for ReservedTimer");
    assert_source_order(
        retire,
        &[
            "TIMERS[owner].with(",
            "owner == current_cpu()?",
            "program_next(&timers.queue)",
        ],
    );

    let pin_helper = source_region(
        source,
        "fn pin_current_timer_cpu(",
        "/// Ends a timer CPU-local transaction",
    );
    assert_source_order(
        pin_helper,
        &["scheduler::preempt_disable()", "current_cpu()"],
    );
    let release_helper = source_region(
        source,
        "fn release_timer_cpu_pin(",
        "fn ensure_initialized(",
    );
    assert!(release_helper.contains("scheduler::preempt_enable_without_reschedule(cpu_pin)"));
}
