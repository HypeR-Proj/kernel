// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::mm::{AddressSpaceResidency, ResidencyError};

#[test]
fn update_cut_blocks_late_entrants_and_targets_all_residents() {
    let mut state = crate::require_ok(AddressSpaceResidency::<4>::try_new(7));
    state
        .check_admission(0, 7)
        .unwrap_or_else(|error| panic!("admission failed: {error:?}"));
    state
        .publish_admission(0)
        .unwrap_or_else(|error| panic!("publish failed: {error:?}"));
    state
        .leave(0, 7)
        .unwrap_or_else(|error| panic!("leave failed: {error:?}"));
    state
        .check_admission(1, 7)
        .unwrap_or_else(|error| panic!("admission failed: {error:?}"));
    state
        .publish_admission(1)
        .unwrap_or_else(|error| panic!("publish failed: {error:?}"));
    assert_eq!(
        state.check_admission(1, 7),
        Err(ResidencyError::AlreadyActive)
    );
    let cut = state
        .begin_update(7)
        .unwrap_or_else(|error| panic!("cut failed: {error:?}"));
    assert_eq!(state.check_admission(2, 7), Err(ResidencyError::Busy));
    assert_eq!(cut.active(), &[false, true, false, false]);
    assert_eq!(cut.targets(), &[true, true, false, false]);
    state
        .finish_update(cut, 8)
        .unwrap_or_else(|error| panic!("finish failed: {error:?}"));
    let second = state
        .begin_update(8)
        .unwrap_or_else(|error| panic!("cut failed: {error:?}"));
    assert_eq!(second.targets(), &[false, true, false, false]);
}

#[test]
fn update_gate_dominates_the_transient_active_epoch() {
    let mut state = crate::require_ok(AddressSpaceResidency::<2>::try_new(4));
    state
        .check_admission(0, 4)
        .unwrap_or_else(|error| panic!("admission failed: {error:?}"));
    state
        .publish_admission(0)
        .unwrap_or_else(|error| panic!("publish failed: {error:?}"));
    let cut = state
        .begin_update(4)
        .unwrap_or_else(|error| panic!("cut failed: {error:?}"));

    // A remote root switch can already have published epoch 5 locally while
    // the coordinator still owns the epoch-4 cut. Leave must retry instead of
    // treating this valid interval as a stale activation.
    assert_eq!(state.leave(0, 5), Err(ResidencyError::Busy));
    state
        .finish_update(cut, 5)
        .unwrap_or_else(|error| panic!("finish failed: {error:?}"));
    state
        .leave(0, 5)
        .unwrap_or_else(|error| panic!("leave failed: {error:?}"));
}

#[test]
fn stale_epoch_cannot_admit_or_start_an_update() {
    let mut state = crate::require_ok(AddressSpaceResidency::<2>::try_new(11));
    assert_eq!(
        state.check_admission(0, 10),
        Err(ResidencyError::StaleEpoch)
    );
    assert!(matches!(
        state.begin_update(10),
        Err(ResidencyError::StaleEpoch)
    ));
}

#[test]
fn retirement_rejects_active_cpus_and_targets_inactive_residents() {
    let mut state = crate::require_ok(AddressSpaceResidency::<2>::try_new(3));
    crate::require_ok(state.check_admission(1, 3));
    crate::require_ok(state.publish_admission(1));
    assert!(matches!(
        state.begin_retirement(3),
        Err(ResidencyError::Busy)
    ));
    crate::require_ok(state.leave(1, 3));
    let cut = crate::require_ok(state.begin_retirement(3));
    assert_eq!(cut.targets(), &[false, true]);
    assert_eq!(state.check_admission(0, 3), Err(ResidencyError::Busy));
    crate::require_ok(state.finish_retirement(cut));
    assert!(state.is_retired());
    assert_eq!(state.check_admission(0, 3), Err(ResidencyError::Retired));
}

#[test]
fn active_epoch_advance_leaves_with_the_current_epoch() {
    let mut state = crate::require_ok(AddressSpaceResidency::<2>::try_new(5));
    crate::require_ok(state.check_admission(0, 5));
    crate::require_ok(state.publish_admission(0));
    crate::require_ok(state.advance_single_active(0, 5, 6));
    crate::require_ok(state.leave(0, 6));

    let retirement = crate::require_ok(state.begin_retirement(6));
    assert_eq!(retirement.targets(), &[true, false]);
}

#[test]
fn inactive_mapping_preflight_rejects_an_active_owner_without_mutation() {
    let mut state = crate::require_ok(AddressSpaceResidency::<2>::try_new(9));
    crate::require_ok(state.check_admission(0, 9));
    crate::require_ok(state.publish_admission(0));
    assert_eq!(state.check_inactive(9), Err(ResidencyError::Busy));
    assert_eq!(state.epoch(), 9);
    crate::require_ok(state.leave(0, 9));
    crate::require_ok(state.check_inactive(9));
}

#[test]
fn update_and_retirement_cuts_are_distinct_linear_types() {
    let source = include_str!("../../../../src/mm/address_space_state.rs");
    assert!(source.contains("pub struct UpdateCut"));
    assert!(source.contains("pub struct RetirementCut"));
    assert!(source.contains("cut: UpdateCut<CPUS>"));
    assert!(source.contains("cut: RetirementCut<CPUS>"));
    assert!(!source.contains("pub struct ResidencyCut"));
}

#[test]
fn update_cut_rejects_another_state_and_returns_exact_authority() {
    let mut first = crate::require_ok(AddressSpaceResidency::<1>::try_new(4));
    let mut second = crate::require_ok(AddressSpaceResidency::<1>::try_new(4));
    let first_cut = crate::require_ok(first.begin_update(4));
    let second_cut = crate::require_ok(second.begin_update(4));

    let failure = match second.finish_update(first_cut, 5) {
        Ok(()) => panic!("foreign update cut was accepted"),
        Err(failure) => failure,
    };
    assert_eq!(failure.error(), ResidencyError::StaleEpoch);
    let first_cut = failure.into_cut();
    crate::require_ok(first.finish_update(first_cut, 5));
    crate::require_ok(second.finish_update(second_cut, 5));
}

#[test]
fn abort_rejects_another_state_and_returns_exact_authority() {
    let mut first = crate::require_ok(AddressSpaceResidency::<1>::try_new(6));
    let mut second = crate::require_ok(AddressSpaceResidency::<1>::try_new(6));
    let first_cut = crate::require_ok(first.begin_update(6));
    let second_cut = crate::require_ok(second.begin_update(6));

    let failure = match second.abort_update(first_cut) {
        Ok(()) => panic!("foreign update cut was accepted by abort"),
        Err(failure) => failure,
    };
    assert_eq!(failure.error(), ResidencyError::StaleEpoch);
    crate::require_ok(first.abort_update(failure.into_cut()));
    crate::require_ok(second.abort_update(second_cut));
}

#[test]
fn retirement_cut_rejects_another_state_and_returns_exact_authority() {
    let mut first = crate::require_ok(AddressSpaceResidency::<1>::try_new(8));
    let mut second = crate::require_ok(AddressSpaceResidency::<1>::try_new(8));
    let first_cut = crate::require_ok(first.begin_retirement(8));
    let second_cut = crate::require_ok(second.begin_retirement(8));

    let failure = match second.finish_retirement(first_cut) {
        Ok(()) => panic!("foreign retirement cut was accepted"),
        Err(failure) => failure,
    };
    assert_eq!(failure.error(), ResidencyError::StaleEpoch);
    let first_cut = failure.into_cut();
    crate::require_ok(first.finish_retirement(first_cut));
    crate::require_ok(second.finish_retirement(second_cut));
}

#[test]
fn stale_finish_returns_the_update_cut_for_retry_or_abort() {
    let mut state = crate::require_ok(AddressSpaceResidency::<1>::try_new(12));
    let cut = crate::require_ok(state.begin_update(12));
    let failure = match state.finish_update(cut, 12) {
        Ok(()) => panic!("non-advancing epoch was accepted"),
        Err(failure) => failure,
    };
    assert_eq!(failure.error(), ResidencyError::StaleEpoch);
    crate::require_ok(state.abort_update(failure.into_cut()));
    crate::require_ok(state.check_admission(0, 12));
}
