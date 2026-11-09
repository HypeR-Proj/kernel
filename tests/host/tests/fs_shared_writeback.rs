// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the production mutable-page admission state. These tests do not
//! claim to model PTE publication, TLB acknowledgement or backend durability.

#[path = "../../../src/fs/vfs/shared/state.rs"]
mod state;
use state::{Phase, State};

#[test]
fn demand_reclaim_does_not_wait_for_its_own_pending_installer() {
    let mut page = State::new();
    let epoch = page.epoch;
    assert_eq!(page.admit(false), Some(epoch));
    assert!(!page.try_close_for_reclaim());
    assert_eq!(page.phase, Phase::Active);
    assert!(
        page.valid(epoch, false),
        "skipping pressure must restore admission"
    );
    page.release(false);
    assert!(page.try_close_for_reclaim());
    assert_eq!(page.captured(true), Some(epoch));
}

#[test]
fn reclaim_skips_busy_writers_and_pins_without_consuming_dirty_generation() {
    let mut page = State::new();
    let epoch = page.epoch;
    assert_eq!(page.admit(true), Some(epoch));
    assert!(!page.try_close_for_reclaim());
    assert!(page.valid(epoch, true));
    assert!(page.dirty);
    page.release(true);
    assert!(page.pin());
    assert!(!page.try_close_for_reclaim());
    assert_eq!(page.phase, Phase::Active);
    assert_eq!(page.epoch, epoch);
    page.unpin();
    assert!(page.try_close_for_reclaim());
    assert_eq!(page.captured(true), Some(epoch));
}

#[test]
fn freeze_invalidates_installation_but_lets_an_admitted_copy_finish() {
    let mut page = State::new();
    let installer = page.epoch;
    assert_eq!(page.admit(false), Some(installer));
    let writer = page.epoch;
    assert_eq!(page.admit(true), Some(writer));
    assert!(!page.close());
    assert_eq!(page.admit(false), None);
    assert_eq!(page.admit(true), None);
    assert!(!page.valid(installer, false));
    assert!(page.valid(writer, true));
    assert_eq!(page.captured(false), None);
    page.release(true);
    assert!(!page.close(), "installer rollback still owns its guard");
    page.release(false);
    assert!(page.close());
    assert_eq!(page.captured(false), Some(writer));
    assert!(!page.valid(writer, true));
    assert!(!page.dirty);
}

#[test]
fn old_snapshot_completion_cannot_validate_or_clear_a_redirtied_generation() {
    let mut page = State::new();
    let old = page.epoch;
    assert_eq!(page.admit(true), Some(old));
    page.release(true);
    assert!(page.close());
    assert_eq!(page.captured(false), Some(old));
    let successor = page.epoch;
    assert_eq!(page.admit(true), Some(successor));
    assert_eq!(successor, old + 1);
    assert!(!page.valid(old, true));
    page.release(true);
    // Completion owns a private old snapshot; State deliberately exposes no
    // completion operation that could clear a later dirty generation.
    assert!(page.dirty);
    assert!(page.close());
    assert_eq!(page.captured(false), Some(successor));
}

#[test]
fn failed_partial_freeze_reopens_without_losing_dirty_bytes_or_generation() {
    let mut first = State::new();
    let mut second = State::new();
    let first_epoch = first.epoch;
    assert_eq!(first.admit(true), Some(first_epoch));
    first.release(true);
    let second_epoch = second.epoch;
    assert_eq!(second.admit(true), Some(second_epoch));
    assert!(first.close());
    assert!(!second.close());
    // A coordinator abandoning a partial withdrawal can reopen every touched
    // page. No capture occurred and existing copies keep their original epoch.
    first.reopen();
    second.reopen();
    assert_eq!(first.epoch, first_epoch);
    assert_eq!(second.epoch, second_epoch);
    assert!(first.dirty && second.dirty);
    assert!(second.valid(second_epoch, true));
    second.release(true);
    assert_eq!(first.admit(false), Some(first_epoch));
    first.release(false);
}

#[test]
fn retirement_rejects_new_aliases_and_cannot_be_reopened() {
    let mut page = State::new();
    assert!(page.close());
    assert_eq!(page.captured(true), Some(1));
    assert_eq!(page.phase, Phase::Retired);
    page.reopen();
    assert_eq!(page.admit(false), None);
    assert_eq!(page.admit(true), None);
    assert!(!page.valid(1, true));
    assert!(!page.valid(page.epoch, false));
    assert_eq!(page.captured(false), None);
}

#[test]
fn failure_invalidates_outstanding_guards_and_never_reopens() {
    let mut page = State::new();
    let write = page.epoch;
    assert_eq!(page.admit(true), Some(write));
    let install = page.epoch;
    assert_eq!(page.admit(false), Some(install));
    page.phase = Phase::Failed;
    assert!(!page.valid(write, true));
    assert!(!page.valid(install, false));
    assert_eq!(page.admit(true), None);
    assert_eq!(page.admit(false), None);
    assert!(!page.close());
    page.release(true);
    page.release(false);
    assert!(page.close());
    page.reopen();
    assert_eq!(page.phase, Phase::Failed);
    assert_eq!(page.captured(false), None);
    assert!(
        page.dirty,
        "failure cannot silently discard unacknowledged bytes"
    );
}

#[test]
fn epoch_exhaustion_preserves_dirty_state_instead_of_wrapping_aba() {
    let mut page = State::new();
    page.epoch = u64::MAX;
    assert_eq!(page.admit(true), Some(u64::MAX));
    page.release(true);
    assert!(page.close());
    assert_eq!(page.captured(false), None);
    assert_eq!(page.epoch, u64::MAX);
    assert!(page.dirty);
    assert_eq!(page.phase, Phase::Freezing);
    page.reopen();
    assert_eq!(page.epoch, u64::MAX);
    assert!(page.dirty);
}

#[test]
fn snapshot_waits_for_every_short_writer_not_just_the_first() {
    let mut page = State::new();
    let first = page.epoch;
    assert_eq!(page.admit(true), Some(first));
    let second = page.epoch;
    assert_eq!(page.admit(true), Some(second));
    assert_eq!(first, second);
    assert!(!page.close());
    page.release(true);
    assert!(!page.close());
    assert!(page.valid(second, true));
    page.release(true);
    assert!(page.close());
    assert_eq!(page.captured(false), Some(first));
}

#[test]
fn retained_copy_plan_blocks_eviction_but_does_not_block_normal_sync() {
    let mut page = State::new();
    assert!(page.pin());
    assert_eq!(page.admit(true), Some(1));
    page.release(true);
    assert_eq!(page.close_for_eviction(), None);
    assert_eq!(page.phase, Phase::Active);
    assert!(
        page.close(),
        "ordinary sync must not wait for blocked IPC plans"
    );
    assert_eq!(page.captured(false), Some(1));
    assert_eq!(page.close_for_eviction(), None);
    page.unpin();
    assert_eq!(page.close_for_eviction(), Some(true));
    assert_eq!(page.captured(true), Some(2));
}

#[test]
fn copy_plan_pin_cannot_join_a_retirement_already_in_progress() {
    let mut page = State::new();
    assert_eq!(page.admit(true), Some(1));
    assert_eq!(page.close_for_eviction(), Some(false));
    assert!(!page.pin());
    assert!(page.valid(1, true));
    page.release(true);
    assert_eq!(page.close_for_eviction(), Some(true));
    assert_eq!(page.captured(true), Some(1));
    assert!(!page.pin());
}

#[test]
fn failure_can_withdraw_translations_while_pinned_backing_stays_owned() {
    let mut page = State::new();
    assert!(page.pin());
    assert!(
        page.close(),
        "poisoning cannot wait on a blocked receiver's pin"
    );
    page.phase = Phase::Failed;
    assert!(!page.pin());
    assert_eq!(page.admit(true), None);
    page.unpin();
    assert_eq!(page.phase, Phase::Failed);
}

#[test]
fn failed_snapshot_submission_restores_latest_dirty_owner_without_rewinding_epoch() {
    for evict in [false, true] {
        let mut page = State::new();
        assert_eq!(page.admit(true), Some(1));
        page.release(true);
        assert!(page.close());
        assert_eq!(page.captured(evict), Some(1));
        assert!(!page.dirty);
        let successor = page.epoch;
        page.submission_failed();
        assert!(page.dirty);
        assert_eq!(page.epoch, successor);
        assert_eq!(page.phase, Phase::Active);
        assert_eq!(page.admit(true), Some(successor));
        page.submission_failed();
        assert!(page.valid(successor, true));
        page.release(true);
        assert!(page.close());
        page.phase = Phase::Failed;
        page.submission_failed();
        assert_eq!(page.phase, Phase::Failed);
        assert!(page.dirty);
    }
}
