// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel log ring-buffer record and wraparound behavior.

use hyper::log::{
    ByteRing, DeferredDrain, DrainBarrierError, DrainBarrierRegistration, DrainBarrierSet,
    DrainBarrierStatus, DrainDisposition, EmergencyQuiescence, EmergencyWriteGate, Level,
    OutputBuffer, ReadResult, RecordFlags, RingBuffer, RuntimeByteAccess, Timestamp,
};

#[test]
fn timestamps_use_linux_width_and_microsecond_precision() {
    assert_eq!(Timestamp::from_microseconds(0).to_string(), "    0.000000");
    assert_eq!(
        Timestamp::from_microseconds(12_000_034).to_string(),
        "   12.000034"
    );
    assert_eq!(
        Timestamp::from_microseconds(123_456_000_007).to_string(),
        "123456.000007"
    );
}

#[test]
fn preserves_record_metadata_across_wraparound() {
    let mut ring = RingBuffer::<64>::new();
    let first = crate::require_ok(ring.append(Level::Info, 123_456, b"first", RecordFlags::NONE));
    let second =
        crate::require_ok(ring.append(Level::Warning, 1_234_567, b"second", RecordFlags::NONE));
    assert_eq!(first, 0);
    assert_eq!(second, 1);

    let mut output = [0; 16];
    let record = match crate::require_ok(ring.read(second, &mut output)) {
        ReadResult::Record(record) => record,
        result => panic!("required a record, received {result:?}"),
    };
    assert_eq!(record.timestamp_microseconds, 1_234_567);
    assert_eq!(record.level, Level::Warning);
    assert_eq!(&output[..record.copied], b"second");

    for index in 0..8u8 {
        crate::require_ok(ring.append(
            Level::Debug,
            u64::from(index),
            &[index; 12],
            RecordFlags::NONE,
        ));
    }
    assert!(ring.dropped() != 0);
    assert!(matches!(
        crate::require_ok(ring.read(first, &mut output)),
        ReadResult::Overrun { .. }
    ));
}

#[test]
fn truncates_a_record_that_exceeds_the_ring_capacity() {
    let mut ring = RingBuffer::<32>::new();
    let sequence = crate::require_ok(ring.append(
        Level::Error,
        0,
        b"a message that cannot fit in this tiny ring",
        RecordFlags::NONE,
    ));
    let mut output = [0; 32];
    let record = match crate::require_ok(ring.read(sequence, &mut output)) {
        ReadResult::Record(record) => record,
        result => panic!("required a record, received {result:?}"),
    };
    assert!(record.flags.contains(RecordFlags::TRUNCATED));
    assert_eq!(record.length, 8);
}

#[test]
fn reports_empty_buffers_and_partial_reads() {
    let mut ring = RingBuffer::<64>::new();
    let mut output = [0; 3];
    assert_eq!(
        crate::require_ok(ring.read(0, &mut output)),
        ReadResult::Empty { next_sequence: 0 }
    );

    let sequence = crate::require_ok(ring.append(Level::Notice, 42, b"abcdef", RecordFlags::NONE));
    let record = match crate::require_ok(ring.read(sequence, &mut output)) {
        ReadResult::Record(record) => record,
        result => panic!("required a record, received {result:?}"),
    };
    assert_eq!(record.length, 6);
    assert_eq!(record.copied, 3);
    assert_eq!(&output, b"abc");
}

#[test]
fn rejects_a_ring_smaller_than_its_record_header() {
    let mut ring = RingBuffer::<8>::new();
    assert_eq!(
        ring.append(Level::Info, 0, b"message", RecordFlags::NONE),
        Err(hyper::log::AppendError::BufferTooSmall)
    );
}

#[test]
fn deferred_drain_coalesces_requests_while_worker_owns_wake() {
    let state = DeferredDrain::new();
    assert!(state.claim_initial_worker());
    assert!(!state.request());
    assert!(!state.request());
    assert!(!state.claim_notification());

    state.begin_batch();
    assert!(!state.request());
    assert_eq!(state.finish_batch(false), DrainDisposition::Continue);

    state.begin_batch();
    assert_eq!(state.finish_batch(false), DrainDisposition::Wait);
}

#[test]
fn deferred_drain_irq_claim_is_retained_for_a_waiter() {
    let state = DeferredDrain::new();
    assert!(state.request());
    assert!(state.consume_prompt());
    assert!(state.claim_notification());
    assert!(!state.claim_notification());

    state.begin_batch();
    assert_eq!(state.finish_batch(false), DrainDisposition::Wait);
    assert!(state.request());
    assert!(state.consume_prompt());
    assert!(state.claim_notification());
}

#[test]
fn deferred_drain_backpressure_hands_wake_to_a_later_irq() {
    let state = DeferredDrain::new();
    assert!(state.claim_initial_worker());
    state.begin_batch();
    assert!(!state.consume_prompt());
    state.defer_until_irq();

    // A producer racing after ownership release coalesces with the prompt
    // published by the blocked worker instead of requiring another wake.
    assert!(!state.request());
    assert!(state.consume_prompt());
    assert!(state.claim_notification());
    assert!(!state.claim_notification());
    state.begin_batch();
    assert_eq!(state.finish_batch(false), DrainDisposition::Wait);
}

#[test]
fn deferred_drain_retains_work_when_a_producer_races_irq_claim() {
    let state = DeferredDrain::new();
    assert!(state.claim_initial_worker());
    state.begin_batch();
    state.defer_until_irq();

    assert!(state.consume_prompt());
    // The IRQ has consumed the worker's prompt but has not claimed ownership.
    // This producer must leave both durable work and a replacement prompt.
    assert!(state.request());
    assert!(state.claim_notification());
    assert!(state.consume_prompt());

    state.begin_batch();
    assert_eq!(state.finish_batch(false), DrainDisposition::Wait);
}

#[test]
fn deferred_drain_coalesces_prompts_until_irq_service_consumes_one() {
    let state = DeferredDrain::new();
    assert!(state.request());
    assert!(!state.request());
    assert!(state.consume_prompt());
    assert!(!state.consume_prompt());
    assert!(state.claim_notification());
    assert!(!state.request());
}

#[test]
fn deferred_drain_request_and_worker_release_have_safe_linearizations() {
    let request_first = DeferredDrain::new();
    assert!(request_first.claim_initial_worker());
    request_first.begin_batch();
    assert!(!request_first.request());
    assert_eq!(
        request_first.finish_batch(false),
        DrainDisposition::Continue
    );

    let release_first = DeferredDrain::new();
    assert!(release_first.claim_initial_worker());
    release_first.begin_batch();
    assert_eq!(release_first.finish_batch(false), DrainDisposition::Wait);
    assert!(release_first.request());
    assert!(!release_first.request());
    assert!(release_first.consume_prompt());
    assert!(release_first.claim_notification());
}

#[test]
fn deferred_drain_request_and_backpressure_have_safe_linearizations() {
    let request_first = DeferredDrain::new();
    assert!(request_first.claim_initial_worker());
    request_first.begin_batch();
    assert!(!request_first.request());
    request_first.defer_until_irq();
    assert!(request_first.consume_prompt());
    assert!(request_first.claim_notification());

    let defer_first = DeferredDrain::new();
    assert!(defer_first.claim_initial_worker());
    defer_first.begin_batch();
    defer_first.defer_until_irq();
    assert!(!defer_first.request());
    assert!(defer_first.consume_prompt());
    assert!(defer_first.claim_notification());
}

#[test]
fn deferred_drain_exhausts_producer_and_irq_step_orders() {
    #[derive(Clone, Copy)]
    enum Step {
        Producer,
        ConsumePrompt,
        ClaimWake,
    }

    let orders = [
        [Step::Producer, Step::ConsumePrompt, Step::ClaimWake],
        [Step::Producer, Step::ClaimWake, Step::ConsumePrompt],
        [Step::ConsumePrompt, Step::Producer, Step::ClaimWake],
        [Step::ConsumePrompt, Step::ClaimWake, Step::Producer],
        [Step::ClaimWake, Step::Producer, Step::ConsumePrompt],
        [Step::ClaimWake, Step::ConsumePrompt, Step::Producer],
    ];

    for order in orders {
        let state = DeferredDrain::new();
        assert!(state.claim_initial_worker());
        state.begin_batch();
        state.defer_until_irq();

        let mut wake_claimed = false;
        for step in order {
            match step {
                Step::Producer => {
                    let _ = state.request();
                }
                Step::ConsumePrompt => {
                    let _ = state.consume_prompt();
                }
                Step::ClaimWake => wake_claimed |= state.claim_notification(),
            }
        }
        if state.consume_prompt() {
            wake_claimed |= state.claim_notification();
        }
        wake_claimed |= state.claim_notification();
        assert!(wake_claimed);

        state.begin_batch();
        assert_eq!(state.finish_batch(false), DrainDisposition::Wait);
    }
}

#[test]
fn registered_drain_barrier_ignores_loss_after_its_target() {
    let mut barriers = DrainBarrierSet::<2>::new();
    let DrainBarrierRegistration::Pending(token) = barriers
        .register(2, 5)
        .unwrap_or_else(|error| panic!("barrier registration failed: {error:?}"))
    else {
        panic!("nonempty watermark completed during registration")
    };

    barriers.advance(5);
    assert!(barriers.take_completion_notification(token.slot()));
    assert!(!barriers.take_completion_notification(token.slot()));
    barriers
        .advance_overrun(5, 8, 3)
        .unwrap_or_else(|error| panic!("overrun accounting failed: {error:?}"));
    assert_eq!(barriers.status(token), Ok(DrainBarrierStatus::Drained));
}

#[test]
fn registered_drain_barrier_completes_an_already_drained_watermark() {
    let mut barriers = DrainBarrierSet::<0>::new();
    assert_eq!(
        barriers.register(5, 5),
        Ok(DrainBarrierRegistration::Complete)
    );
}

#[test]
fn registered_drain_barrier_retains_only_intersecting_loss() {
    let mut barriers = DrainBarrierSet::<2>::new();
    let DrainBarrierRegistration::Pending(token) = barriers
        .register(2, 7)
        .unwrap_or_else(|error| panic!("barrier registration failed: {error:?}"))
    else {
        panic!("nonempty watermark completed during registration")
    };

    barriers
        .advance_overrun(2, 5, 3)
        .unwrap_or_else(|error| panic!("overrun accounting failed: {error:?}"));
    barriers.advance(7);
    barriers
        .advance_overrun(7, 9, 2)
        .unwrap_or_else(|error| panic!("later overrun accounting failed: {error:?}"));
    assert_eq!(
        barriers.status(token),
        Ok(DrainBarrierStatus::Overrun { missed: 3 })
    );
}

#[test]
fn registered_drain_barrier_excludes_loss_before_registration() {
    let mut barriers = DrainBarrierSet::<1>::new();
    let token = match barriers.register(5, 8) {
        Ok(DrainBarrierRegistration::Pending(token)) => token,
        result => panic!("unexpected barrier registration result: {result:?}"),
    };
    barriers
        .advance_overrun(2, 8, 6)
        .unwrap_or_else(|error| panic!("overrun accounting failed: {error:?}"));
    assert_eq!(
        barriers.status(token),
        Ok(DrainBarrierStatus::Overrun { missed: 3 })
    );
}

#[test]
fn registered_drain_barrier_rejects_stale_tokens_and_bad_loss_ranges() {
    let mut barriers = DrainBarrierSet::<1>::new();
    let DrainBarrierRegistration::Pending(first) = barriers
        .register(0, 1)
        .unwrap_or_else(|error| panic!("barrier registration failed: {error:?}"))
    else {
        panic!("nonempty watermark completed during registration")
    };
    assert_eq!(barriers.register(0, 2), Err(DrainBarrierError::NoFreeSlot));
    assert_eq!(barriers.release(first), Ok(()));
    let DrainBarrierRegistration::Pending(second) = barriers
        .register(0, 2)
        .unwrap_or_else(|error| panic!("barrier reuse failed: {error:?}"))
    else {
        panic!("nonempty watermark completed during reuse")
    };
    assert_eq!(barriers.status(first), Err(DrainBarrierError::InvalidToken));
    assert_ne!(first, second);
    assert_eq!(
        barriers.advance_overrun(0, 3, 2),
        Err(DrainBarrierError::InvalidLossRange)
    );
}

#[test]
fn byte_ring_preserves_fifo_order_and_counts_overflow() {
    let mut queue = ByteRing::<3>::new();
    assert!(queue.push(1));
    assert!(queue.push(2));
    assert!(queue.push(3));
    assert_eq!(queue.remaining_capacity(), 0);
    assert!(!queue.push(4));
    assert_eq!(queue.dropped(), 1);

    let mut first = [0; 2];
    assert_eq!(queue.pop_into(&mut first), 2);
    assert_eq!(first, [1, 2]);
    assert_eq!(queue.remaining_capacity(), 2);
    assert!(queue.push(5));

    let mut remainder = [0; 3];
    assert_eq!(queue.pop_into(&mut remainder), 2);
    assert_eq!(&remainder[..2], &[3, 5]);
    assert!(queue.is_empty());
}

#[test]
fn zero_capacity_byte_ring_remains_well_defined() {
    let mut queue = ByteRing::<0>::new();
    assert!(!queue.push(1));
    assert_eq!(queue.dropped(), 1);
    assert_eq!(queue.pop_into(&mut [0; 1]), 0);
}

#[test]
fn byte_ring_front_is_retained_until_explicitly_accepted() {
    let mut queue = ByteRing::<2>::new();
    assert!(queue.push(7));
    assert!(queue.push(8));
    assert_eq!(queue.front(), Some(7));
    assert_eq!(queue.front(), Some(7));
    assert_eq!(queue.pop_front(), Some(7));
    assert_eq!(queue.front(), Some(8));
}

#[test]
fn byte_ring_peek_requires_an_explicit_prefix_commit() {
    let mut queue = ByteRing::<4>::new();
    assert!(queue.push(1));
    assert!(queue.push(2));
    assert!(queue.push(3));

    let mut observed = [0; 2];
    assert_eq!(queue.peek_into(&mut observed), 2);
    assert_eq!(observed, [1, 2]);
    assert_eq!(queue.front(), Some(1));
    assert!(!queue.discard_front(4));
    assert_eq!(queue.front(), Some(1));
    assert!(queue.discard_front(2));
    assert_eq!(queue.front(), Some(3));
}

#[test]
fn byte_ring_transaction_prefix_survives_storage_wraparound() {
    let mut queue = ByteRing::<3>::new();
    assert!(queue.push(1));
    assert!(queue.push(2));
    assert_eq!(queue.pop_front(), Some(1));
    assert!(queue.push(3));
    assert!(queue.push(4));

    let mut observed = [0; 3];
    assert_eq!(queue.peek_into(&mut observed), 3);
    assert_eq!(observed, [2, 3, 4]);
    assert!(queue.discard_front(2));
    assert_eq!(queue.front(), Some(4));
}

#[test]
fn byte_ring_treats_console_control_bytes_as_opaque_data() {
    let mut queue = ByteRing::<6>::new();
    assert!(queue.push(b'x'));
    assert!(queue.push(b'y'));
    assert_eq!(queue.pop_front(), Some(b'x'));
    assert_eq!(queue.pop_front(), Some(b'y'));
    for byte in *b"a\n\0\rde" {
        assert!(queue.push(byte));
    }

    let mut short = [0; 3];
    assert_eq!(queue.peek_into(&mut short), short.len());
    assert_eq!(&short, b"a\n\0");
    assert_eq!(queue.front(), Some(b'a'));

    let mut complete = [0; 6];
    assert_eq!(queue.peek_into(&mut complete), complete.len());
    assert_eq!(&complete, b"a\n\0\rde");
}

#[test]
fn output_buffer_retains_rejected_and_unattempted_bytes() {
    let mut output = OutputBuffer::<8>::new();
    assert!(output.push_console_bytes(b"a\nb").is_ok());
    let mut accepted = [0; 4];
    let mut count = 0;
    let first = output.try_write(4, |byte| {
        if count == 2 {
            return false;
        }
        accepted[count] = byte;
        count += 1;
        true
    });
    assert_eq!(first.accepted, 2);
    assert!(!first.complete);
    assert!(first.blocked);
    assert_eq!(&accepted[..2], b"a\r");
    assert_eq!(output.remaining(), 2);

    let second = output.try_write(1, |byte| {
        accepted[count] = byte;
        count += 1;
        true
    });
    assert_eq!(second.accepted, 1);
    assert!(!second.complete);
    assert!(!second.blocked);
    assert_eq!(output.remaining(), 1);
    let final_progress = output.try_write(4, |byte| {
        accepted[count] = byte;
        count += 1;
        true
    });
    assert!(final_progress.complete);
    assert!(!final_progress.blocked);
    assert_eq!(&accepted, b"a\r\nb");
}

#[test]
fn output_buffer_distinguishes_sink_backpressure_from_budget_exhaustion() {
    let mut output = OutputBuffer::<2>::new();
    assert!(output.push_bytes(b"xy").is_ok());

    let budget = output.try_write(1, |_| true);
    assert_eq!(budget.accepted, 1);
    assert!(!budget.complete);
    assert!(!budget.blocked);

    let blocked = output.try_write(1, |_| false);
    assert_eq!(blocked.accepted, 0);
    assert!(!blocked.complete);
    assert!(blocked.blocked);
    assert_eq!(output.remaining(), 1);
}

#[test]
fn emergency_gate_serializes_normal_byte_permits() {
    let gate = EmergencyWriteGate::new();
    let first = match gate.try_begin_normal_byte(2) {
        RuntimeByteAccess::Acquired(permit) => permit,
        _ => panic!("first normal byte permit was not acquired"),
    };
    assert!(matches!(
        gate.try_begin_normal_byte(2),
        RuntimeByteAccess::Busy
    ));
    drop(first);
    assert!(matches!(
        gate.try_begin_normal_byte(2),
        RuntimeByteAccess::Acquired(_)
    ));
}

#[test]
fn emergency_gate_busy_contender_cannot_replace_the_active_cpu() {
    let gate = EmergencyWriteGate::new();
    let permit = match gate.try_begin_normal_byte(1) {
        RuntimeByteAccess::Acquired(permit) => permit,
        _ => panic!("normal byte permit was not acquired"),
    };
    assert!(matches!(
        gate.try_begin_normal_byte(0),
        RuntimeByteAccess::Busy
    ));
    assert_eq!(
        gate.retire_normal_writer(0, 0),
        EmergencyQuiescence::RemoteOwnerTimedOut
    );
    assert!(!gate.emergency_enabled());
    drop(permit);
}

#[test]
fn emergency_gate_retires_an_idle_normal_writer() {
    let gate = EmergencyWriteGate::new();
    assert_eq!(
        gate.retire_normal_writer(0, 0),
        EmergencyQuiescence::Quiescent
    );
    assert!(gate.emergency_enabled());
    assert!(matches!(
        gate.try_begin_normal_byte(1),
        RuntimeByteAccess::Retired
    ));
}

#[test]
fn emergency_gate_abandons_a_locally_interrupted_byte() {
    let gate = EmergencyWriteGate::new();
    let permit = match gate.try_begin_normal_byte(3) {
        RuntimeByteAccess::Acquired(permit) => permit,
        _ => panic!("normal byte permit was not acquired"),
    };
    assert_eq!(
        gate.retire_normal_writer(3, 0),
        EmergencyQuiescence::LocalOwnerAbandoned
    );
    assert!(gate.emergency_enabled());
    drop(permit);
}

#[test]
fn emergency_gate_fails_closed_on_a_stalled_remote_writer() {
    let gate = EmergencyWriteGate::new();
    let permit = match gate.try_begin_normal_byte(1) {
        RuntimeByteAccess::Acquired(permit) => permit,
        _ => panic!("normal byte permit was not acquired"),
    };
    assert_eq!(
        gate.retire_normal_writer(0, 4),
        EmergencyQuiescence::RemoteOwnerTimedOut
    );
    assert!(!gate.emergency_enabled());
    assert!(matches!(
        gate.try_begin_normal_byte(2),
        RuntimeByteAccess::Retired
    ));

    drop(permit);
    assert_eq!(
        gate.retire_normal_writer(0, 0),
        EmergencyQuiescence::Quiescent
    );
    assert!(gate.emergency_enabled());
}
