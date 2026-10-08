// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded scenarios using production mechanisms with instrumented atomics.
//! Observe payloads before joining publishers: a join would supply the very
//! synchronization edge these tests need the mechanism itself to establish.

#![cfg(test)]

#[path = "../../../src/sync/deferred_work.rs"]
mod deferred_work;
#[path = "../../../src/kernel/task/reschedule.rs"]
mod reschedule;
#[path = "../../../src/kernel/vm/run_admission.rs"]
mod run_admission;

mod admission;
mod publication;
mod wakeup;

fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected failure: {error:?}"),
    }
}
