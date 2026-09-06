// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral Native syscall payload and result invariants.

use hyper::abi::native::{
    HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL, HYPER_NATIVE_STATUS_FAULT,
    HYPER_NATIVE_SYS_BYTE_CHANNEL_READ, HYPER_NATIVE_SYS_CAPABILITY_CHANNEL_RECEIVE,
    NativeInvocation, NativeResult,
};

#[test]
fn invocation_owns_the_complete_machine_payload() {
    let invocation = NativeInvocation::new(17, [1, 2, 3, 4, 5, 6], 0x8000);

    assert_eq!(invocation.number(), 17);
    assert_eq!(invocation.arguments(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(invocation.call_site(), 0x8000);
}

#[test]
fn failed_results_clear_all_auxiliary_words() {
    let result = NativeResult::new(-1, [0xfeed, 0xbeef]);

    assert_eq!(result.status(), -1);
    assert_eq!(result.values(), &[0, 0]);
}

#[test]
fn successful_results_preserve_auxiliary_words() {
    let result = NativeResult::new(0, [0xfeed, 0xbeef]);

    assert_eq!(result.status(), 0);
    assert_eq!(result.values(), &[0xfeed, 0xbeef]);
}

#[test]
fn declared_byte_channel_read_failure_preserves_required_size() {
    let result = NativeResult::for_syscall(
        HYPER_NATIVE_SYS_BYTE_CHANNEL_READ,
        HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
        [4096, 0],
    );

    assert_eq!(result.status(), HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
    assert_eq!(result.values(), &[4096, 0]);
}

#[test]
fn undeclared_byte_channel_read_failure_clears_auxiliary_words() {
    let result = NativeResult::for_syscall(
        HYPER_NATIVE_SYS_BYTE_CHANNEL_READ,
        HYPER_NATIVE_STATUS_FAULT,
        [4096, 3],
    );

    assert_eq!(result.status(), HYPER_NATIVE_STATUS_FAULT);
    assert_eq!(result.values(), &[0, 0]);
}

#[test]
fn declared_capability_receive_failure_preserves_both_required_counts() {
    let result = NativeResult::for_syscall(
        HYPER_NATIVE_SYS_CAPABILITY_CHANNEL_RECEIVE,
        HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL,
        [4096, 16],
    );

    assert_eq!(result.status(), HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
    assert_eq!(result.values(), &[4096, 16]);
}

#[test]
fn capability_receive_fault_does_not_publish_auxiliary_values() {
    let result = NativeResult::for_syscall(
        HYPER_NATIVE_SYS_CAPABILITY_CHANNEL_RECEIVE,
        HYPER_NATIVE_STATUS_FAULT,
        [4096, 16],
    );

    assert_eq!(result.status(), HYPER_NATIVE_STATUS_FAULT);
    assert_eq!(result.values(), &[0, 0]);
}
