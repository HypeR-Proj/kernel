// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise reclamation while this test thread's allocator rejects every request.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use hyper_trace::{Error, Filter, Recorder};

#[derive(Clone, Copy, Default)]
struct State {
    deny: bool,
    attempts: usize,
    freed: usize,
}

thread_local! {
    // Constant, destructor-free TLS avoids allocation inside allocator callbacks.
    static STATE: Cell<State> = const { Cell::new(State { deny: false, attempts: 0, freed: 0 }) };
}

struct Allocator;

// SAFETY: Successful allocations and every deallocation preserve System's layout
// and pointer contracts. Failure injection is thread-local and returns null.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let denied = STATE
            .try_with(|cell| {
                let mut state = cell.get();
                if state.deny {
                    state.attempts = state.attempts.saturating_add(1);
                    cell.set(state);
                }
                state.deny
            })
            .unwrap_or(false);
        if denied {
            core::ptr::null_mut()
        } else {
            // SAFETY: Forward the caller's valid allocation layout unchanged.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = STATE.try_with(|cell| {
            let mut state = cell.get();
            if state.deny {
                state.freed = state.freed.saturating_add(layout.size());
                cell.set(state);
            }
        });
        // SAFETY: This pointer came from System with this exact layout.
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn without_allocations<T>(operation: impl FnOnce() -> T) -> (T, State) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STATE.set(State::default());
        }
    }
    STATE.set(State {
        deny: true,
        ..State::default()
    });
    let reset = Reset;
    let result = operation();
    let state = STATE.get();
    drop(reset);
    (result, state)
}

#[test]
fn trace_clear_reclaims_buffers_when_all_new_allocations_fail() {
    let recorder = Recorder::<2, 16384>::new();
    let session = recorder
        .start(Filter::ALL)
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    let (active, state) = without_allocations(|| recorder.clear(session));
    assert_eq!(active, Err(Error::Busy));
    assert_eq!((state.attempts, state.freed), (0, 0));
    recorder
        .stop(session)
        .unwrap_or_else(|error| panic!("stop: {error:?}"));
    let (cleared, state) = without_allocations(|| recorder.clear(session));
    assert_eq!(cleared, Ok(()));
    assert_eq!(state.attempts, 0);
    assert!(state.freed >= 2 * 16384 * core::mem::size_of::<Option<hyper_trace::Record>>());
    let (stale, state) = without_allocations(|| recorder.clear(session));
    assert_eq!(stale, Err(Error::InvalidSession));
    assert_eq!((state.attempts, state.freed), (0, 0));
}

#[test]
fn trace_detach_reclaims_only_after_the_retired_owner_is_dropped() {
    let recorder = Recorder::<2, 16384>::new();
    // Replacement must install fresh retirement slots as well as fresh payloads.
    for capacity in [64, 16384] {
        let session = recorder
            .start_with_capacity(Filter::ALL, capacity)
            .unwrap_or_else(|error| panic!("start: {error:?}"));
        recorder
            .stop(session)
            .unwrap_or_else(|error| panic!("stop: {error:?}"));
    }
    let session = recorder
        .start(Filter::ALL)
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    recorder
        .stop(session)
        .unwrap_or_else(|error| panic!("stop: {error:?}"));
    let (retired, state) = without_allocations(|| recorder.detach(session));
    assert_eq!((state.attempts, state.freed), (0, 0));
    let retired = retired.unwrap_or_else(|error| panic!("detach: {error:?}"));
    let mut records = [None; 1];
    assert!(matches!(
        recorder.read(session, 0, 0, &mut records),
        Err(Error::InvalidSession)
    ));
    let ((), state) = without_allocations(|| drop(retired));
    assert_eq!(state.attempts, 0);
    assert!(state.freed >= 2 * 16384 * core::mem::size_of::<Option<hyper_trace::Record>>());
    let next = recorder
        .start(Filter::ALL)
        .unwrap_or_else(|error| panic!("restart: {error:?}"));
    assert!(next.generation() > session.generation());
    recorder
        .stop(next)
        .unwrap_or_else(|error| panic!("stop: {error:?}"));
    let (cleared, state) = without_allocations(|| recorder.clear(next));
    assert_eq!(cleared, Ok(()));
    assert_eq!(state.attempts, 0);
}
