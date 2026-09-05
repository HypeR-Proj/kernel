// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process and user-thread lifecycle transaction tests.

#[path = "../../../../src/kernel/process/lifecycle.rs"]
mod lifecycle;

use lifecycle::{
    LifecycleError, ProcessLifecycle, ProcessPhase, StopDispatchProgress, TerminalReason,
    UserThreadLifecycle, UserThreadPhase,
};

use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn stop_before_publication_cannot_reopen_thread_admission() {
    let mut process = ProcessLifecycle::prepared();
    assert!(process.request_stop(TerminalReason::Requested));
    assert_eq!(process.phase(), ProcessPhase::Stopped);
    assert_eq!(process.publish(), Err(LifecycleError::AlreadyPublished));
    assert_eq!(
        process.reserve_thread(),
        Err(LifecycleError::AdmissionClosed)
    );
}

#[test]
fn admitted_thread_may_publish_after_stop_but_keeps_terminal_outcome() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.reserve_thread(), Ok(()));
    let fault = TerminalReason::Fault {
        class: 3,
        code: 0xfeed,
    };
    assert!(process.request_stop(fault));
    assert_eq!(process.phase(), ProcessPhase::Stopping);
    assert_eq!(process.publish_thread(), Ok(()));
    assert_eq!(process.terminal(), Some(fault));
    assert_eq!(process.detach_thread(0), Ok(()));
    assert_eq!(process.phase(), ProcessPhase::Stopped);
    assert_eq!(process.terminal(), Some(fault));
}

#[test]
fn aborting_the_last_pending_thread_completes_a_stop() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.reserve_thread(), Ok(()));
    assert!(process.request_stop(TerminalReason::Requested));
    assert_eq!(process.phase(), ProcessPhase::Stopping);
    assert_eq!(process.abort_thread(), Ok(true));
    assert_eq!(process.phase(), ProcessPhase::Stopped);
    assert_eq!(
        process.abort_thread(),
        Err(LifecycleError::InvalidMembership)
    );
}

#[test]
fn first_terminal_reason_wins_every_later_stop_race() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.start(), Ok(()));
    let first = TerminalReason::Fault { class: 7, code: 11 };
    assert!(process.request_stop(first));
    assert!(!process.request_stop(TerminalReason::Requested));
    assert_eq!(process.terminal(), Some(first));
}

#[test]
fn explicit_thread_exit_supplies_the_last_thread_status() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.start(), Ok(()));
    assert_eq!(process.reserve_thread(), Ok(()));
    assert_eq!(process.publish_thread(), Ok(()));
    assert_eq!(process.detach_thread(-17), Ok(()));
    assert_eq!(
        process.terminal(),
        Some(TerminalReason::LastThreadExited { status: -17 })
    );
}

#[test]
fn explicit_process_exit_is_an_immutable_process_outcome() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.start(), Ok(()));
    let exit = TerminalReason::ProcessExited { status: 23 };
    assert!(process.request_stop(exit));
    assert!(!process.request_stop(TerminalReason::Requested));
    assert_eq!(process.terminal(), Some(exit));
}

#[test]
fn user_thread_terminal_and_detach_are_exactly_once() {
    let mut thread = UserThreadLifecycle::prepared();
    assert_eq!(thread.publish(), Ok(()));
    assert_eq!(thread.mark_runnable(), Ok(()));
    let exit = TerminalReason::ThreadExited { status: -17 };
    assert!(thread.request_terminal(exit));
    assert_eq!(thread.terminal(), Some(exit));
    assert!(!thread.request_terminal(TerminalReason::Fault { class: 1, code: 2 }));
    assert_eq!(thread.detach(), Ok(exit));
    assert_eq!(thread.phase(), UserThreadPhase::Detached);
    assert_eq!(thread.detach(), Err(LifecycleError::InvalidMembership));
}

#[test]
fn user_thread_normal_return_is_not_a_process_terminal_reason() {
    let mut thread = UserThreadLifecycle::prepared();
    assert_eq!(thread.publish(), Ok(()));
    assert_eq!(
        thread.detach(),
        Ok(TerminalReason::ThreadExited { status: 0 })
    );
}

#[test]
fn retirement_requires_quiescence_and_is_exactly_once() {
    let mut process = ProcessLifecycle::prepared();
    assert_eq!(process.begin_retirement(), Err(LifecycleError::NotStopped));
    assert_eq!(process.publish(), Ok(()));
    assert_eq!(process.start(), Ok(()));
    assert_eq!(process.reserve_thread(), Ok(()));
    assert_eq!(process.pending_threads(), 1);
    assert_eq!(process.publish_thread(), Ok(()));
    assert_eq!(process.pending_threads(), 0);
    assert_eq!(process.active_threads(), 1);

    let reason = TerminalReason::TaskGroupStop { generation: 9 };
    assert!(process.request_stop(reason));
    assert_eq!(process.begin_retirement(), Err(LifecycleError::NotStopped));
    assert_eq!(process.detach_thread(0), Ok(()));
    assert_eq!(process.active_threads(), 0);
    assert_eq!(process.phase(), ProcessPhase::Stopped);
    assert_eq!(process.terminal(), Some(reason));

    assert_eq!(process.begin_retirement(), Ok(()));
    assert_eq!(process.phase(), ProcessPhase::Retiring);
    assert_eq!(process.finish_retirement(), Ok(()));
    assert_eq!(process.phase(), ProcessPhase::Retired);
    assert_eq!(
        process.begin_retirement(),
        Err(LifecycleError::AlreadyRetired)
    );
    assert_eq!(process.finish_retirement(), Err(LifecycleError::NotStopped));
}

#[test]
fn pending_admission_keeps_stop_dispatch_incomplete() {
    let mut dispatch = StopDispatchProgress::new(1);
    assert!(!dispatch.is_complete());
    assert_eq!(dispatch.incomplete(), 1);

    dispatch.observe(true);
    assert_eq!(dispatch.incomplete(), 1);
    dispatch.observe(false);
    assert_eq!(dispatch.incomplete(), 2);
}

#[test]
fn detached_record_retains_successor_for_concurrent_stop_scan() {
    struct Record {
        id: u8,
        next: RefCell<Option<Rc<Record>>>,
    }

    let tail = Rc::new(Record {
        id: 3,
        next: RefCell::new(None),
    });
    let middle = Rc::new(Record {
        id: 2,
        next: RefCell::new(Some(Rc::clone(&tail))),
    });
    let head = Rc::new(Record {
        id: 1,
        next: RefCell::new(Some(Rc::clone(&middle))),
    });
    let authoritative = RefCell::new(Some(head));

    // A scanner may retain `head` while the authoritative list unlinks it.
    // The removed record's published successor must remain intact until that
    // scanner advances, exactly as Process and TaskGroup records require.
    let retained = authoritative.borrow().as_ref().map(Rc::clone);
    let authoritative_head = retained
        .as_ref()
        .and_then(|record| record.next.borrow().clone());
    *authoritative.borrow_mut() = authoritative_head;

    let retained_successor = retained
        .as_ref()
        .and_then(|record| record.next.borrow().clone());
    assert_eq!(retained_successor.as_ref().map(|record| record.id), Some(2));
    assert_eq!(
        authoritative.borrow().as_ref().map(|record| record.id),
        Some(2)
    );
}
