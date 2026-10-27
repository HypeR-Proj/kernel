// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Manual timer queue scaling measurement; never a CI timing assertion.
//!
//! Run with `cargo run --release --manifest-path kernel/tests/host/Cargo.toml
//! --example timer_queue`. Allocation and destruction are outside measured
//! intervals, matching the runtime queue lock boundary.

extern crate alloc;
#[path = "../../../src/time/queue/mod.rs"]
#[allow(dead_code, unused_imports)]
mod queue;

use std::hint::black_box;
use std::time::{Duration, Instant};
use {queue::OwnedDeadlineQueue, queue::PendingTimer, queue::TimerEvent, queue::TimerMode};

fn callback(_: TimerEvent, _: usize) {}

fn main() -> Result<(), queue::TimerQueueError> {
    println!("timers,operations,insert_ns,reset_ns,cancel_ns");
    for count in [1_usize, 32, 256, 4096] {
        let batches = (16_384 / count).max(4);
        let mut insert = Duration::ZERO;
        let mut reset = Duration::ZERO;
        let mut cancel = Duration::ZERO;
        let mut queue = OwnedDeadlineQueue::new();
        let mut nodes = Vec::with_capacity(count);
        let mut handles = Vec::with_capacity(count);
        for index in 0..count {
            nodes.push(PendingTimer::try_new(
                index as u64,
                TimerMode::OneShot,
                callback,
                index,
            )?);
        }
        for _ in 0..batches {
            let start = Instant::now();
            while let Some(node) = nodes.pop() {
                handles.push(queue.insert(black_box(node), 0));
            }
            insert += start.elapsed();

            // Multiplication by an odd number permutes these power-of-two
            // populations, so resets visit every live identity in mixed order.
            let start = Instant::now();
            for step in 0..count {
                let index = step.wrapping_mul(97) & (count - 1);
                queue.reschedule(handles[index], step as u64, 0)?;
            }
            reset += start.elapsed();

            let start = Instant::now();
            for step in 0..count {
                let index = step.wrapping_mul(113) & (count - 1);
                nodes.push(queue.cancel(black_box(handles[index]))?);
            }
            cancel += start.elapsed();
            handles.clear();
        }
        let operations = count * batches;
        let cost = |duration: Duration| duration.as_nanos() as f64 / operations as f64;
        println!(
            "{count},{operations},{:.2},{:.2},{:.2}",
            cost(insert),
            cost(reset),
            cost(cancel)
        );
    }
    Ok(())
}
