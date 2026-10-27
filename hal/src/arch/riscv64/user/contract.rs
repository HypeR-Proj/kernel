// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure native-user trap classification, independent of page-table ownership.

use hyper::hal::user::UserFaultKind;

/// Classifies a captured U-mode synchronous trap.
///
/// RISC-V does not distinguish absent translations, permission failures, or
/// software-managed access/dirty-bit faults in `scause`. Page-fault variants
/// are candidates only: the kernel must check access and make mapping progress
/// before retrying. Access faults and interrupts never reach the pager.
pub(crate) const fn fault_kind(cause: u64) -> UserFaultKind {
    match cause {
        0 | 4 | 6 => UserFaultKind::Alignment,
        1 => UserFaultKind::InstructionAbort,
        12 => UserFaultKind::ExecutePageFault,
        5 | 7 => UserFaultKind::DataAbort,
        13 => UserFaultKind::ReadPageFault,
        15 => UserFaultKind::WritePageFault,
        2 => UserFaultKind::IllegalInstruction,
        3 => UserFaultKind::Breakpoint,
        _ => UserFaultKind::OtherSynchronous,
    }
}
