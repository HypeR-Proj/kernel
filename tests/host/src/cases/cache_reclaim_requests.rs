// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::RequestState;

#[test]
fn completion_before_wait_is_retained() {
    let mut state = RequestState::new();
    let request = crate::require_some(state.begin(8));
    assert!(!state.completed(request.generation));
    assert!(state.finish(request.generation));
    assert!(state.completed(request.generation));
}

#[test]
fn delayed_completion_cannot_finish_same_order_replacement() {
    let mut state = RequestState::new();
    let old = crate::require_some(state.begin(8));
    assert!(state.finish(old.generation));
    let new = crate::require_some(state.begin(8));
    assert!(!state.finish(old.generation));
    assert!(!state.completed(new.generation));
    assert_eq!(state.pending(), Some(new));
    assert!(state.finish(new.generation));
}

#[test]
fn active_request_and_exhausted_generation_cannot_be_replaced() {
    let mut state = RequestState::new();
    let request = crate::require_some(state.begin(7));
    assert!(state.begin(9).is_none());
    assert_eq!(state.pending(), Some(request));
    assert!(state.finish(request.generation));
    state.next = u64::MAX;
    assert!(state.begin(1).is_none());
    assert!(state.pending().is_none());
}

#[test]
fn cancelled_sweep_cannot_complete_a_newer_request() {
    let mut state = RequestState::new();
    let cancelled = crate::require_some(state.begin(3));
    // The caller's guard retires this generation while an old worker has
    // already copied its request and detached a batch outside the lock.
    assert!(state.finish(cancelled.generation));
    let replacement = crate::require_some(state.begin(12));
    assert!(!state.finish(cancelled.generation));
    assert!(!state.completed(replacement.generation));
    assert_eq!(state.pending(), Some(replacement));
}
