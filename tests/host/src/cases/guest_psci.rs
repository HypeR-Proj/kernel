// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest power handoff and restart contracts independent of hardware execution.

#[allow(dead_code)]
#[path = "../../../../src/vm/arm/psci.rs"]
mod model;
use model::*;

fn state(count: usize) -> PowerState {
    let mut state = crate::require_some(PowerState::new(count));
    crate::require_ok(state.boot());
    state
}

fn on(state: &mut PowerState, source: u32, target: u32) -> Request {
    crate::require_ok(state.stage(
        source,
        Operation::CpuOn,
        [u64::from(target), 0x1000, 0xfeed],
        0x1000..0x2000,
    ));
    assert_eq!(state.publish(source), Ok(true));
    let request = crate::require_some(state.pending());
    assert_eq!(request.vcpu, source);
    request
}

fn start(state: &mut PowerState, source: u32, target: u32) {
    let request = on(state, source, target);
    crate::require_ok(state.complete(request.id, true));
    assert_eq!(
        state.continuation(source),
        Ok(Continuation::Resume(SUCCESS))
    );
    assert_eq!(
        state.continuation(target),
        Ok(Continuation::Restart(Bootstrap {
            entry: 0x1000,
            context: 0xfeed
        }))
    );
}

#[test]
fn power_on_is_reserved_before_publication_and_completed_exactly_once() {
    let mut state = state(3);
    start(&mut state, 0, 1);
    crate::require_ok(state.stage(0, Operation::CpuOn, [2, 0x1000, 7], 0x1000..0x2000));
    assert_eq!(state.pending(), None);
    assert_eq!(state.affinity(2, 0), 2);
    assert_eq!(
        state.stage(1, Operation::CpuOn, [2, 0x1000, 9], 0x1000..0x2000),
        Err(ON_PENDING)
    );
    assert_eq!(state.continuation(0), Ok(Continuation::Wait));
    assert_eq!(state.publish(0), Ok(true));
    assert_eq!(state.publish(0), Ok(false));
    let request = crate::require_some(state.pending());
    assert_eq!(
        state.complete(request.id + 1, true),
        Err(INVALID_PARAMETERS)
    );
    crate::require_ok(state.complete(request.id, true));
    assert_eq!(state.complete(request.id, true), Err(INVALID_PARAMETERS));
    assert_eq!(state.affinity(2, 0), 0);
    assert_eq!(
        state.continuation(2),
        Ok(Continuation::Restart(Bootstrap {
            entry: 0x1000,
            context: 7
        }))
    );
    assert_eq!(state.continuation(2), Ok(Continuation::Wait));
    assert_eq!(state.continuation(0), Ok(Continuation::Resume(SUCCESS)));
}

#[test]
fn rejected_power_on_releases_target_and_uses_a_psci_1_0_error() {
    let mut state = state(2);
    let request = on(&mut state, 0, 1);
    crate::require_ok(state.complete(request.id, false));
    assert_eq!(state.affinity(1, 0), 1);
    assert_eq!(state.continuation(1), Ok(Continuation::Wait));
    assert_eq!(
        state.continuation(0),
        Ok(Continuation::Resume(INTERNAL_FAILURE))
    );
    assert!(on(&mut state, 0, 1).id > request.id);
}

#[test]
fn successful_cpu_off_never_resumes_old_call_and_can_restart_with_new_context() {
    let mut state = state(2);
    start(&mut state, 0, 1);
    crate::require_ok(state.stage(1, Operation::CpuOff, [0; 3], 0x1000..0x2000));
    assert_eq!(state.affinity(1, 0), 0);
    assert_eq!(
        state.stage(0, Operation::CpuOn, [1, 0x1000, 0], 0x1000..0x2000),
        Err(ALREADY_ON)
    );
    crate::require_ok(state.publish(1));
    let request = crate::require_some(state.pending());
    crate::require_ok(state.complete(request.id, true));
    assert_eq!(state.affinity(1, 0), 1);
    assert_eq!(state.continuation(1), Ok(Continuation::Wait));
    start(&mut state, 0, 1);
}

#[test]
fn denied_cpu_off_resumes_and_system_calls_cannot_be_resumed_by_completion() {
    let mut state = state(1);
    crate::require_ok(state.stage(0, Operation::CpuOff, [0; 3], 0x1000..0x2000));
    crate::require_ok(state.publish(0));
    let request = crate::require_some(state.pending());
    crate::require_ok(state.complete(request.id, false));
    assert_eq!(state.continuation(0), Ok(Continuation::Resume(DENIED)));
    for operation in [Operation::SystemOff, Operation::SystemReset] {
        let mut state = self::state(1);
        crate::require_ok(state.stage(0, operation, [0; 3], 0x1000..0x2000));
        crate::require_ok(state.publish(0));
        let request = crate::require_some(state.pending());
        assert_eq!(state.complete(request.id, true), Err(INVALID_PARAMETERS));
        assert_eq!(state.complete(request.id, false), Err(INVALID_PARAMETERS));
        assert_eq!(state.pending(), Some(request));
        assert_eq!(state.continuation(0), Ok(Continuation::Wait));
    }
}

#[test]
fn topology_entry_validation_and_higher_affinity_queries_are_bounded() {
    assert!(PowerState::new(0).is_none());
    assert!(PowerState::new(MAX_CPUS + 1).is_none());
    let mut state = state(2);
    assert_eq!(
        state.stage(0, Operation::CpuOn, [2, 0x1000, 0], 0x1000..0x2000),
        Err(INVALID_PARAMETERS)
    );
    for entry in [0xffc, 0x1001, 0x2000, u64::MAX] {
        assert_eq!(
            state.stage(0, Operation::CpuOn, [1, entry, 0], 0x1000..0x2000),
            Err(INVALID_ADDRESS)
        );
    }
    assert_eq!(state.pending(), None);
    assert_eq!(state.affinity(1, 0), 1);
    assert_eq!(state.affinity(2, 0), INVALID_PARAMETERS);
    assert_eq!(state.affinity(0xff, 1), 0);
    assert_eq!(state.affinity(0xffff, 2), 0);
    assert_eq!(state.affinity(0xff_ffff, 3), 0);
    assert_eq!(state.affinity(0x100, 1), INVALID_PARAMETERS);
    assert_eq!(state.affinity(1 << 32, 3), INVALID_PARAMETERS);
    assert_eq!(state.affinity(0, 4), INVALID_PARAMETERS);
    assert_eq!(state.publish(2), Err(INVALID_PARAMETERS));
    assert_eq!(state.continuation(2), Err(INVALID_PARAMETERS));
}

#[test]
fn psci_error_registers_follow_the_signed_32_bit_result_contract() {
    for (result, encoded) in [
        (SUCCESS, 0),
        (NOT_SUPPORTED, 0xffff_ffff),
        (INVALID_PARAMETERS, 0xffff_fffe),
        (DENIED, 0xffff_fffd),
        (ALREADY_ON, 0xffff_fffc),
        (ON_PENDING, 0xffff_fffb),
        (INTERNAL_FAILURE, 0xffff_fffa),
        (INVALID_ADDRESS, 0xffff_fff7),
    ] {
        assert_eq!(return_register(result), encoded);
    }
}
