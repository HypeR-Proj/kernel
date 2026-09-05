// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Active stage-2 mapping commit-point classification.

use core::cell::Cell;

use hyper::cpu::CpuIndex;
use hyper::vm::translation::{
    ActiveMappingError, ExclusiveExecution, ExecutionError, ExecutionReleaseFailure,
    publish_active_mapping, residency_is_current,
};

fn release_error(result: Result<(), ExecutionReleaseFailure>) -> Option<ExecutionError> {
    match result {
        Ok(()) => None,
        Err(failure) => {
            let error = failure.error();
            // Preserve test progress even though the production capability is
            // deliberately fail-stop when abandoned while armed.
            core::mem::forget(failure.into_claim());
            Some(error)
        }
    }
}

#[test]
fn install_failure_is_reversible_and_skips_invalidation() {
    let mut state = (Cell::new(false), Cell::new(false));
    let result = publish_active_mapping(
        &mut state,
        |state| {
            state.0.set(true);
            Err::<(), _>(11)
        },
        |state| {
            state.1.set(true);
            Ok(())
        },
    );

    assert_eq!(result, Err(ActiveMappingError::BeforeInstall(11)));
    assert!(state.0.get());
    assert!(!state.1.get());
}

#[test]
fn invalidation_failure_reports_a_committed_mapping() {
    let mut installed = Cell::new(false);
    let result = publish_active_mapping(
        &mut installed,
        |installed| {
            installed.set(true);
            Ok(())
        },
        |_| Err::<(), _>(23),
    );

    assert!(installed.get());
    assert_eq!(
        result,
        Err(ActiveMappingError::InstalledButInvalidationFailed(23))
    );
}

#[test]
fn exclusive_execution_serializes_cross_cpu_ownership() {
    let execution = ExclusiveExecution::new(0x1234);
    let cpu0 = CpuIndex::new(0).unwrap_or(CpuIndex::BOOT);
    let cpu1 = CpuIndex::new(1).unwrap_or(CpuIndex::BOOT);

    let first = execution.claim(cpu0);
    assert!(first.is_ok());
    assert!(matches!(
        execution.claim(cpu1),
        Err(ExecutionError::AlreadyActive)
    ));
    let Ok(first) = first else {
        return;
    };
    assert_eq!(first.cpu(), cpu0);
    assert_eq!(release_error(execution.release(first, cpu0)), None);

    let migrated = execution.claim(cpu1);
    assert!(migrated.is_ok());
    let Ok(migrated) = migrated else {
        return;
    };
    assert_eq!(release_error(execution.release(migrated, cpu1)), None);
}

#[test]
fn execution_claim_cannot_release_another_address_space() {
    let first = ExclusiveExecution::new(1);
    let second = ExclusiveExecution::new(2);
    let cpu = CpuIndex::new(0).unwrap_or(CpuIndex::BOOT);
    let claim = second.claim(cpu);
    assert!(claim.is_ok());
    let Ok(claim) = claim else {
        return;
    };
    let failure = first.release(claim, cpu);
    assert!(failure.is_err());
    let Err(failure) = failure else {
        return;
    };
    assert_eq!(failure.error(), ExecutionError::WrongAddressSpace);
    assert_eq!(
        release_error(second.release(failure.into_claim(), cpu)),
        None
    );
}

#[test]
fn wrong_cpu_release_preserves_active_ownership() {
    let execution = ExclusiveExecution::new(3);
    let cpu0 = CpuIndex::new(0).unwrap_or(CpuIndex::BOOT);
    let cpu1 = CpuIndex::new(1).unwrap_or(CpuIndex::BOOT);
    let claim = execution.claim(cpu0);
    assert!(claim.is_ok());
    let Ok(claim) = claim else {
        return;
    };

    let failure = execution.release(claim, cpu1);
    assert!(failure.is_err());
    let Err(failure) = failure else {
        return;
    };
    assert_eq!(failure.error(), ExecutionError::WrongCpu);
    assert!(matches!(
        execution.claim(cpu0),
        Err(ExecutionError::AlreadyActive)
    ));
    assert_eq!(
        release_error(execution.release(failure.into_claim(), cpu0)),
        None
    );
}

#[test]
fn zero_root_never_aliases_the_unobserved_epoch() {
    assert!(!residency_is_current(0, 0, 0, 1));
    assert!(residency_is_current(0, 1, 0, 1));
    assert!(!residency_is_current(0, 1, 0, 2));
}
