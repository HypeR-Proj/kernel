// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/vfs/lock_state.rs"]
mod model;

use model::{Admission, Grants, LockMode, OwnerState};

#[test]
fn readers_share_but_independent_writer_waits() {
    let mut grants = Grants::new();
    let mut first = OwnerState::new();
    let mut second = OwnerState::new();
    let mut writer = OwnerState::new();
    assert_eq!(
        grants.request(&mut first, LockMode::Shared, false),
        Admission::Granted
    );
    assert_eq!(
        grants.request(&mut second, LockMode::Shared, false),
        Admission::Granted
    );
    assert_eq!(
        grants.request(&mut writer, LockMode::Exclusive, false),
        Admission::Wait
    );
    grants.release(&mut first);
    assert!(!grants.compatible(LockMode::Exclusive));
    grants.release(&mut second);
    assert_eq!(
        grants.request(&mut writer, LockMode::Exclusive, false),
        Admission::Granted
    );
    assert!(!grants.compatible(LockMode::Shared));
    grants.release(&mut writer);
    assert!(grants.compatible(LockMode::Exclusive));
}

#[test]
fn queued_writer_prevents_reader_barging() {
    let mut grants = Grants::new();
    let mut first = OwnerState::new();
    let mut late = OwnerState::new();
    assert_eq!(
        grants.request(&mut first, LockMode::Shared, false),
        Admission::Granted
    );
    assert_eq!(
        grants.request(&mut late, LockMode::Shared, true),
        Admission::Wait
    );
    // Idempotence does not add another reader grant and may not deadlock behind
    // a queued writer which is waiting for this very reader to unlock.
    assert_eq!(
        grants.request(&mut first, LockMode::Shared, true),
        Admission::Granted
    );
    grants.release(&mut first);
    assert!(grants.compatible(LockMode::Exclusive));
}

#[test]
fn upgrade_preserves_shared_lock_on_contention() {
    let mut grants = Grants::new();
    let mut first = OwnerState::new();
    let mut second = OwnerState::new();
    grants.request(&mut first, LockMode::Shared, false);
    grants.request(&mut second, LockMode::Shared, false);
    assert_eq!(
        grants.request(&mut first, LockMode::Exclusive, false),
        Admission::Busy
    );
    assert_eq!(first.held(), Some(LockMode::Shared));
    grants.release(&mut second);
    assert_eq!(
        grants.request(&mut first, LockMode::Exclusive, true),
        Admission::Busy
    );
    assert_eq!(first.held(), Some(LockMode::Shared));
    assert_eq!(
        grants.request(&mut first, LockMode::Exclusive, false),
        Admission::Granted
    );
    assert!(!grants.compatible(LockMode::Shared));
    assert_eq!(
        grants.request(&mut first, LockMode::Shared, true),
        Admission::Granted
    );
    assert!(grants.compatible(LockMode::Shared));
    assert!(!grants.compatible(LockMode::Exclusive));
    grants.release(&mut first);
    assert!(grants.compatible(LockMode::Exclusive));
}

#[test]
fn close_revokes_grant_and_permanently_rejects_resolved_operations() {
    let mut grants = Grants::new();
    let mut owner = OwnerState::new();
    grants.request(&mut owner, LockMode::Exclusive, false);
    grants.close(&mut owner);
    assert!(owner.closed());
    assert_eq!(owner.held(), None);
    assert!(grants.compatible(LockMode::Exclusive));
    assert_eq!(
        grants.request(&mut owner, LockMode::Shared, false),
        Admission::Closed
    );
    grants.close(&mut owner);
    assert!(grants.compatible(LockMode::Exclusive));
}

#[test]
fn pending_cleanup_cannot_be_overtaken_by_new_owner_request() {
    let mut grants = Grants::new();
    let mut owner = OwnerState::new();
    owner.set_pending(true);
    assert_eq!(
        grants.request(&mut owner, LockMode::Shared, false),
        Admission::Busy
    );
    grants.grant(&mut owner, LockMode::Shared);
    assert_eq!(
        grants.request(&mut owner, LockMode::Exclusive, false),
        Admission::Busy
    );
    // Unlock may precede the winning waiter's return, but cannot permit a new
    // pending request until the old continuation has unlinked its queue node.
    grants.release(&mut owner);
    assert!(owner.pending());
    assert_eq!(
        grants.request(&mut owner, LockMode::Exclusive, false),
        Admission::Busy
    );
    owner.set_pending(false);
    assert_eq!(
        grants.request(&mut owner, LockMode::Exclusive, false),
        Admission::Granted
    );
    grants.close(&mut owner);
}

#[test]
fn closed_pending_owner_remains_closed_after_waiter_retirement() {
    let mut grants = Grants::new();
    let mut owner = OwnerState::new();
    owner.set_pending(true);
    grants.close(&mut owner);
    assert!(owner.pending());
    owner.set_pending(false);
    assert_eq!(
        grants.request(&mut owner, LockMode::Exclusive, false),
        Admission::Closed
    );
    assert_eq!(OwnerState::from_bits(owner.bits()), Some(owner));
    for invalid in [3, 5, 6, 7, 16, 255] {
        assert_eq!(OwnerState::from_bits(invalid), None);
    }
}
