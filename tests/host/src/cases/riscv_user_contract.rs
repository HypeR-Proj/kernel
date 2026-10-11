// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::riscv_user_contract_model::fault_kind;
use hyper::hal::user::UserFaultKind;

#[test]
fn user_page_faults_preserve_access_type_and_exclude_access_faults() {
    let expected = [
        UserFaultKind::Alignment,        // instruction address misaligned
        UserFaultKind::InstructionAbort, // instruction access fault
        UserFaultKind::IllegalInstruction,
        UserFaultKind::Breakpoint,
        UserFaultKind::Alignment,        // load address misaligned
        UserFaultKind::DataAbort,        // load access fault
        UserFaultKind::Alignment,        // store/AMO address misaligned
        UserFaultKind::DataAbort,        // store/AMO access fault
        UserFaultKind::OtherSynchronous, // environment calls use another path
        UserFaultKind::OtherSynchronous,
        UserFaultKind::OtherSynchronous,
        UserFaultKind::OtherSynchronous,
        UserFaultKind::ExecutePageFault,
        UserFaultKind::ReadPageFault,
        UserFaultKind::OtherSynchronous,
        UserFaultKind::WritePageFault,
    ];
    for (cause, expected) in expected.into_iter().enumerate() {
        assert_eq!(fault_kind(cause as u64), expected, "scause={cause}");
    }
    for cause in 16..64 {
        assert_eq!(fault_kind(cause), UserFaultKind::OtherSynchronous);
    }
}

#[test]
fn interrupts_are_not_page_faults_even_if_low_cause_bits_match() {
    for cause in 0..64 {
        assert_eq!(
            fault_kind((1 << 63) | cause),
            UserFaultKind::OtherSynchronous
        );
    }
}
