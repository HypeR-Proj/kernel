// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded scenarios using production mechanisms with instrumented atomics.
//! Observe payloads before joining publishers: a join would supply the very
//! synchronization edge these tests need the mechanism itself to establish.

#![cfg(test)]

#[path = "../../../core/src/sync/deferred_work.rs"]
mod deferred_work;
#[path = "../../../src/task/reschedule.rs"]
mod reschedule;
#[path = "../../../src/vm/run_admission.rs"]
mod run_admission;

mod admission;
mod publication;
mod wakeup;

// The lock keeps the production mask guard; only its atomic lanes are modeled.
use hyper::hal;
use hyper::sync::InterruptMaskGuard;
mod balance_continuation;
#[path = "../../../src/task/scheduler/balance/retry.rs"]
mod balance_retry;
#[path = "../../../src/task/scheduler/balance/work.rs"]
mod balance_work;
mod scheduler_lock;
#[path = "../../../core/src/sync/lock/sharded.rs"]
mod sharded_lock;

fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected failure: {error:?}"),
    }
}

#[path = "../../../src/fs/vfs/mount_admission.rs"]
mod mount_admission;
mod mounts;

mod protection;
#[path = "../../../src/fs/vfs/protection_state.rs"]
mod protection_state;

#[path = "../../../src/diagnostics/profile/admission.rs"]
mod profile_admission;
mod profiling;

mod guest_entry;
#[path = "../../../src/vm/reconcile.rs"]
mod reconcile;
