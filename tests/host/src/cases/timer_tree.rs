// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Timer tree balance, ownership and extended-deadline regression tests.

use std::collections::BTreeMap;
use {
    crate::time::queue::OwnedDeadlineQueue, crate::time::queue::PendingTimer,
    crate::time::queue::TimerEvent, crate::time::queue::TimerHandle, crate::time::queue::TimerMode,
};

fn callback(_: TimerEvent, _: usize) {}

fn pending(deadline: u64, context: usize) -> PendingTimer {
    crate::require_ok(PendingTimer::try_new(
        deadline,
        TimerMode::OneShot,
        callback,
        context,
    ))
}

#[test]
fn every_small_deletion_order_preserves_balance_and_minima() {
    fn check(order: &mut [usize], start: usize) {
        if start != order.len() {
            for index in start..order.len() {
                order.swap(index, start);
                check(order, start + 1);
                order.swap(index, start);
            }
            return;
        }
        let mut queue = OwnedDeadlineQueue::new();
        let handles: Vec<_> = (0..order.len())
            .map(|id| queue.insert(pending(id as u64, id), 0))
            .collect();
        assert!(queue.validate_tree());
        let mut live = vec![true; order.len()];
        for &index in order.iter() {
            drop(crate::require_ok(queue.cancel(handles[index])));
            live[index] = false;
            assert!(
                queue.validate_tree(),
                "deletion order {order:?}, removed {index}"
            );
            assert_eq!(
                queue.next_deadline(),
                live.iter().position(|present| *present).map(|id| id as u64)
            );
        }
    }
    check(&mut [0, 1, 2, 3, 4, 5, 6], 0);
}

#[test]
fn mixed_mutations_match_sorted_reference_across_counter_wrap() {
    struct Entry {
        handle: TimerHandle,
        deadline: i128,
        sequence: u64,
    }
    let mut model: BTreeMap<usize, Entry> = BTreeMap::new();
    let mut queue = OwnedDeadlineQueue::new();
    let mut stale = Vec::new();
    let mut random = 0x1234_5678_9abc_def0_u64;
    let mut sequence = 0;
    let mut next_id = 0;
    let mut elapsed = 0_i128;
    let origin = u64::MAX - 1000;
    for step in 0..40_000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        elapsed += 1;
        let now = origin.wrapping_add(elapsed as u64);
        let offset = (random % 2000) as i128 - 500;
        let deadline = elapsed + offset;
        let raw_deadline = origin.wrapping_add(deadline as u64);
        match (random >> 32) % 4 {
            0 if model.len() < 256 => {
                sequence += 1;
                let id = next_id;
                next_id += 1;
                let handle = queue.insert(pending(raw_deadline, id), now);
                model.insert(
                    id,
                    Entry {
                        handle,
                        deadline,
                        sequence,
                    },
                );
            }
            1 | 2 if !model.is_empty() => {
                let index = (random as usize) % model.len();
                let id = *crate::require_some(model.keys().nth(index));
                let entry = crate::require_some(model.get_mut(&id));
                if random & 1 == 0 {
                    sequence += 1;
                    crate::require_ok(queue.reschedule(entry.handle, raw_deadline, now));
                    entry.deadline = deadline;
                    entry.sequence = sequence;
                } else {
                    drop(crate::require_ok(queue.cancel(entry.handle)));
                    stale.push(entry.handle);
                    model.remove(&id);
                }
            }
            _ => {
                let first = model
                    .iter()
                    .min_by_key(|(_, entry)| (entry.deadline, entry.sequence))
                    .map(|(&id, entry)| (id, entry.deadline));
                let expired = queue.pop_expired(now);
                match first {
                    Some((id, deadline)) if deadline <= elapsed => {
                        let expired = crate::require_some(expired);
                        let expected = crate::require_some(model.remove(&id));
                        assert_eq!(expired.context(), id);
                        assert_eq!(expired.event().handle, expected.handle);
                        assert_eq!(
                            expired.event().deadline,
                            origin.wrapping_add(deadline as u64)
                        );
                        stale.push(expected.handle);
                        expired.invoke();
                    }
                    _ => assert!(expired.is_none()),
                }
            }
        }
        assert!(queue.validate_tree(), "step {step}");
        let minimum = model
            .values()
            .min_by_key(|entry| (entry.deadline, entry.sequence));
        assert_eq!(
            queue.next_deadline(),
            minimum.map(|entry| origin.wrapping_add(entry.deadline as u64)),
            "step {step}"
        );
        assert_eq!(queue.stats().active_timers, model.len());
        if let Some(handle) = stale.get((random as usize) % stale.len().max(1)) {
            assert!(queue.cancel(*handle).is_err());
        }
    }
}

#[test]
fn overdue_and_far_future_timers_do_not_form_a_wrapping_comparison_cycle() {
    let mut queue = OwnedDeadlineQueue::new();
    let old = queue.insert(pending(99, 1), 100);
    let future = 100_u64.wrapping_add(i64::MAX as u64);
    let new = queue.insert(pending(future, 2), 100);
    assert_eq!(queue.next_deadline(), Some(99));
    assert_eq!(
        crate::require_some(queue.pop_expired(100)).event().handle,
        old
    );
    assert!(queue.pop_expired(100).is_none());
    assert_eq!(queue.next_deadline(), Some(future));
    assert_eq!(
        crate::require_some(queue.pop_expired(future))
            .event()
            .handle,
        new
    );
    assert!(queue.validate_tree());
}

#[test]
fn periodic_rearm_keeps_phase_with_an_overdue_backlog() {
    let mut queue = OwnedDeadlineQueue::new();
    let interval = i64::MAX as u64;
    let periodic = queue.insert(
        crate::require_ok(PendingTimer::try_new(
            10,
            TimerMode::Periodic { interval },
            callback,
            1,
        )),
        10,
    );
    let old = queue.insert(pending(11, 2), 10);
    let current = queue.insert(pending(20, 3), 20);
    assert_eq!(
        crate::require_some(queue.pop_expired(20)).event().handle,
        periodic
    );
    assert_eq!(queue.next_deadline(), Some(11));
    assert_eq!(
        crate::require_some(queue.pop_expired(20)).event().handle,
        old
    );
    assert_eq!(
        crate::require_some(queue.pop_expired(20)).event().handle,
        current
    );
    assert_eq!(queue.next_deadline(), Some(10 + interval));
    drop(crate::require_ok(queue.cancel(periodic)));
}

#[test]
fn extended_time_retains_overdue_timers_across_multiple_observations() {
    let mut queue = OwnedDeadlineQueue::new();
    let handle = queue.insert(pending(0, 1), 0);
    // Resetting another timer advances queue time without draining the old
    // timer. The old raw deadline eventually looks future under signed compare.
    let other = queue.insert(pending(1, 2), 0);
    crate::require_ok(queue.reschedule(other, u64::MAX - 1, i64::MAX as u64));
    crate::require_ok(queue.reschedule(other, 10, u64::MAX - 1));
    assert_eq!(queue.next_deadline(), Some(0));
    assert_eq!(queue.next_programming_deadline(), Some(u64::MAX - 1));
    assert_eq!(
        crate::require_some(queue.pop_expired(u64::MAX - 1))
            .event()
            .handle,
        handle
    );
    assert!(queue.pop_expired(u64::MAX - 1).is_none());
    assert_eq!(queue.next_programming_deadline(), Some(10));
    assert_eq!(
        crate::require_some(queue.pop_expired(10)).event().handle,
        other
    );
}

#[test]
fn reset_equal_deadline_moves_timer_to_end_of_fifo() {
    let mut queue = OwnedDeadlineQueue::new();
    let first = queue.insert(pending(10, 1), 0);
    let second = queue.insert(pending(10, 2), 0);
    crate::require_ok(queue.reschedule(first, 10, 0));
    assert_eq!(
        crate::require_some(queue.pop_expired(10)).event().handle,
        second
    );
    assert_eq!(
        crate::require_some(queue.pop_expired(10)).event().handle,
        first
    );
}

#[test]
fn large_tree_handles_recycled_allocations_and_adversarial_deletion() {
    const COUNT: usize = 4096;
    for stride in [1, COUNT - 1, 97] {
        let mut queue = OwnedDeadlineQueue::new();
        let mut handles: Vec<_> = (0..COUNT)
            .map(|id| queue.insert(pending(id as u64, id), 0))
            .collect();
        // Recycling alternately removes internal/leaf nodes and inserts new
        // identities, while retaining a large outstanding population.
        for step in 0..COUNT {
            let index = step * 113 % COUNT;
            let node = crate::require_ok(queue.cancel(handles[index]));
            handles[index] = queue.insert(node, 0);
            crate::require_ok(queue.reschedule(handles[index], index as u64, 0));
            if step % 31 == 0 {
                assert!(queue.validate_tree());
            }
        }
        for step in 0..COUNT {
            let index = step * stride % COUNT;
            drop(crate::require_ok(queue.cancel(handles[index])));
            if step % 31 == 0 {
                assert!(queue.validate_tree());
            }
        }
        assert!(queue.validate_tree());
        assert_eq!(queue.next_deadline(), None);
    }
}

#[test]
fn ancient_periodic_overruns_saturate_without_losing_next_deadline() {
    let mut queue = OwnedDeadlineQueue::new();
    let periodic = queue.insert(
        crate::require_ok(PendingTimer::try_new(
            0,
            TimerMode::Periodic { interval: 1 },
            callback,
            1,
        )),
        0,
    );
    let other = queue.insert(pending(1, 2), 0);
    let mut now = 0_u64;
    for _ in 0..3 {
        now = now.wrapping_add(i64::MAX as u64);
        crate::require_ok(queue.reschedule(other, now.wrapping_add(10), now));
    }
    let expired = crate::require_some(queue.pop_expired(now));
    assert_eq!(expired.event().handle, periodic);
    assert_eq!(expired.event().overruns, u64::MAX);
    assert_eq!(queue.next_deadline(), Some(now.wrapping_add(1)));
    assert!(queue.validate_tree());
}
