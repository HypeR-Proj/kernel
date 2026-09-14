// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[allow(dead_code)]
#[path = "../../../../src/kernel/vm/io/model.rs"]
mod model;

#[test]
fn guest_io_receive_fault_keeps_original_record_and_backpressure() {
    let mut state = model::MailboxState::new();
    state.staging[..3].copy_from_slice(b"abc");
    state.staging_length = 3;
    crate::require_ok(state.commit_guest());
    let mut bytes = [0; model::RECORD_BYTES];
    let (length, sequence) = crate::require_ok(state.claim(&mut bytes));
    assert_eq!(&bytes[..length], b"abc");
    assert_eq!(state.commit_guest(), Err(model::Error::Busy));
    state.finish_claim(sequence, false);
    let (_, retry_sequence) = crate::require_ok(state.claim(&mut bytes));
    assert_eq!(sequence, retry_sequence);
    state.finish_claim(sequence, true);
    assert_ne!(state.guest_status() & model::TX_SPACE, 0);
}

#[test]
fn guest_io_stale_consume_cannot_discard_new_control_record() {
    let mut state = model::MailboxState::new();
    crate::require_ok(state.send(b"first"));
    let first = state.outgoing.sequence;
    crate::require_ok(state.consume(first));
    crate::require_ok(state.send(b"second"));
    assert_eq!(state.consume(first), Err(model::Error::Invalid));
    assert_eq!(&state.outgoing.data[..state.outgoing.length], b"second");
}

#[test]
fn guest_io_notifications_survive_pre_enable_completion_and_consume_race() {
    let mut state = model::NotificationState::new();
    state.call((u64::from(state.epoch) << 32) | 1);
    assert!(!state.front_irq());
    crate::require_ok(state.control(1));
    assert!(state.front_irq());
    state.kick(2);
    assert_eq!(state.take_kicks(), 4);
    state.kick(2);
    assert!(state.back_irq());
    assert_eq!(state.take_kicks(), 4);
    assert!(!state.back_irq());
}

#[test]
fn guest_io_reset_rejects_stale_completion_and_close_is_terminal() {
    let mut state = model::NotificationState::new();
    let old = crate::require_ok(state.control(1));
    state.kick(0);
    let next = crate::require_ok(state.control(0));
    assert_ne!(old, next);
    assert_eq!(crate::require_ok(state.control(0)), next);
    state.call((u64::from(old) << 32) | 3);
    crate::require_ok(state.control(1));
    assert!(!state.front_irq());
    assert!(!state.back_irq());
    state.close();
    assert_eq!(state.control(1), Err(model::Error::Closed));
    state.call((u64::from(next) << 32) | 3);
    assert!(!state.front_irq());
}

#[test]
fn guest_io_virtio_ack_does_not_consume_backend_kicks() {
    let mut state = model::NotificationState::new();
    crate::require_ok(state.control(1));
    state.kick(1);
    state.call((u64::from(state.epoch) << 32) | 3);
    state.ack(1);
    assert_eq!(state.status, 2);
    assert_eq!(state.take_kicks(), 2);
}

#[test]
fn guest_io_disabled_backend_can_raise_needs_reset_config_irq() {
    let mut state = model::NotificationState::new();
    crate::require_ok(state.control(2));
    assert!(state.front_irq());
    assert!(!state.back_irq());
    state.ack(2);
    assert!(!state.front_irq());
}

#[test]
fn guest_io_peer_loss_allows_final_config_irq_but_never_reactivates() {
    let mut state = model::NotificationState::new();
    let epoch = crate::require_ok(state.control(1));
    state.kick(2);
    state.close();
    assert_eq!(state.take_kicks(), 0);
    assert!(!state.front_irq()); // Native must publish NEEDS_RESET first.
    crate::require_ok(state.control(2));
    assert!(state.front_irq());
    state.close(); // An independent peer-retirement notification is idempotent.
    assert!(state.front_irq());
    state.ack(2);
    assert!(!state.front_irq());
    state.call((u64::from(epoch) << 32) | 3);
    assert!(!state.front_irq());
    assert_eq!(state.control(1), Err(model::Error::Closed));
}

#[test]
fn guest_io_copy_claim_is_valid_across_stop_and_abort() {
    let mut state = model::MailboxState::new();
    state.staging[..4].copy_from_slice(b"data");
    state.staging_length = 4;
    crate::require_ok(state.commit_guest());
    let mut bytes = [0; model::RECORD_BYTES];
    let (length, sequence) = crate::require_ok(state.claim(&mut bytes));
    state.closed = true;
    assert_eq!(&bytes[..length], b"data");
    state.finish_claim(sequence, false);
    assert_eq!(state.native_status(), 1 | 4);
    let (_, retry) = crate::require_ok(state.claim(&mut bytes));
    assert_eq!(retry, sequence);
    state.finish_claim(sequence, true);
    assert_eq!(state.native_status(), 4);
    assert_eq!(state.claim(&mut bytes), Err(model::Error::Closed));
}
