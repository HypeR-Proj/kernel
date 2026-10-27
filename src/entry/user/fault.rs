// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduled, cancellable refill of protected file mappings and private pages.

use hyper::hal::user::UserFaultKind;

use crate::mm::reclaim;
use {
    crate::mm::user_space::Access, crate::mm::user_space::MachineError,
    crate::mm::user_space::NativeAddressSpace, crate::mm::user_space::UserAddress,
};
use {crate::process::UserThread, crate::process::UserThreadPhase};

/// Called only after the machine run and its execution pin have been released.
/// Failed preparations drop their temporary pages before scheduled reclamation.
/// A write fault may have populated clean pages before COW fails; retrying that
/// preparation preserves the unchanged user registers and logical contents.
pub(super) fn resolve(
    thread: &UserThread,
    space: &NativeAddressSpace,
    address: UserAddress,
    kind: UserFaultKind,
    observed_epoch: u64,
) -> Result<bool, MachineError> {
    crate::tracepoint!(
        Memory,
        "page_fault_begin",
        thread = thread.scheduler_id().map_or(0, |id| id.get()),
        kind = kind as u64
    );
    let cancelled = || thread.snapshot().phase == UserThreadPhase::StopRequested;
    let result = reclaim::retry_prepare_with(
        || {
            if cancelled() {
                return Ok(false);
            }
            match kind {
                UserFaultKind::ReadPageFault => {
                    space.resolve_file_fault(address, Access::Read, observed_epoch)
                }
                UserFaultKind::ExecutePageFault => {
                    space.resolve_file_fault(address, Access::Execute, observed_epoch)
                }
                UserFaultKind::WritePageFault => {
                    match space.resolve_file_fault(address, Access::Write, observed_epoch) {
                        Ok(false) => space.resolve_write_fault(address, observed_epoch),
                        result => result,
                    }
                }
                _ => Ok(false),
            }
        },
        |error| {
            if cancelled() {
                None
            } else {
                reclaim::user_memory_target(error)
            }
        },
    );
    crate::tracepoint!(
        Memory,
        "page_fault_end",
        thread = thread.scheduler_id().map_or(0, |id| id.get()),
        resolved = u64::from(matches!(result, Ok(true)))
    );
    result
}
