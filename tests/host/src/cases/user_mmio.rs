// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::vm::device::mmio::{Error, PendingMmio};
use hyper::vm::exit::{AccessWidth, GuestPhysicalAddress, MmioAccess, MmioAction, MmioOperation};

fn read() -> MmioAccess {
    MmioAccess::new(
        GuestPhysicalAddress::new(0x0a00_0000),
        AccessWidth::Word,
        MmioOperation::Read,
    )
}

#[test]
fn mmio_is_not_visible_until_hardware_detach_publication() {
    let mut state = PendingMmio::new();
    crate::require_ok(state.stage(7, read()));
    assert_eq!(state.pending(), None);
    assert_eq!(
        state.complete(1, MmioAction::CompleteRead(9)),
        Err(Error::Stale)
    );
    crate::require_ok(state.publish());
    let request = crate::require_some(state.pending());
    assert_eq!(request.device, 7);
    assert_eq!(request.access, read());
    assert_eq!(state.pending(), Some(request));
    assert_eq!(state.stage(8, read()), Err(Error::Busy));
    assert_eq!(
        state.complete(request.id, MmioAction::CompleteWrite),
        Err(Error::WrongCompletion)
    );
    assert_eq!(state.pending(), Some(request));
    crate::require_ok(state.complete(request.id, MmioAction::CompleteRead(9)));
    assert_eq!(state.pending(), None);
    assert_eq!(state.stage(8, read()), Err(Error::Busy));
    assert_eq!(
        state.take_completed(),
        Ok(Some(MmioAction::CompleteRead(9)))
    );
    assert_eq!(state.take_completed(), Ok(None));
    crate::require_ok(state.stage(8, read()));
    crate::require_ok(state.publish());
    assert_ne!(crate::require_some(state.pending()).id, request.id);
    assert_eq!(
        state.complete(request.id, MmioAction::CompleteRead(9)),
        Err(Error::Stale)
    );
}

#[test]
fn mmio_close_invalidates_staged_pending_and_completed_work() {
    for phase in 0..3 {
        let mut state = PendingMmio::new();
        crate::require_ok(state.stage(1, read()));
        if phase > 0 {
            crate::require_ok(state.publish());
        }
        if phase > 1 {
            crate::require_ok(state.complete(1, MmioAction::CompleteRead(0)));
        }
        state.close();
        assert_eq!(state.pending(), None);
        assert_eq!(
            state.complete(1, MmioAction::CompleteRead(0)),
            Err(Error::Closed)
        );
        assert_eq!(state.take_completed(), Err(Error::Closed));
        assert_eq!(state.stage(1, read()), Err(Error::Closed));
        assert_eq!(state.publish(), Err(Error::Closed));
    }
}

#[test]
fn vcpu_requests_are_independent_and_failure_is_explicit() {
    let mut first = PendingMmio::new();
    let mut second = PendingMmio::new();
    crate::require_ok(first.stage(1, read()));
    crate::require_ok(second.stage(2, read()));
    crate::require_ok(first.publish());
    crate::require_ok(second.publish());
    crate::require_ok(first.complete(1, MmioAction::Stop));
    assert!(second.pending().is_some());
    assert_eq!(first.take_completed(), Ok(Some(MmioAction::Stop)));
}
