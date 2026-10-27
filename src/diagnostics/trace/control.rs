// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Privileged capture lifecycle. Control never runs from a producer; copyout
//! happens after releasing every recorder/control lock. Expiry is a callout,
//! not a timer tick or an operation added to ordinary interrupt dispatch.

use super::Result;
use hyper::abi::native::*;
use hyper::sync::{
    InterruptSpinLock, PublishedOnce,
    atomic::{AtomicU64, Ordering},
};
use {crate::time, crate::time::Callout, crate::time::CalloutRef, crate::time::TimerEvent};
use {hyper_trace as trace, hyper_trace::Filter, hyper_trace::Session};

struct Capture {
    session: Option<Session>,
    started: u64,
    deadline: u64,
    categories: u64,
    capacity: u64,
    running: bool,
    cpus: [u64; 4],
}
static CONTROL: InterruptSpinLock<Capture, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(Capture {
        session: None,
        started: 0,
        deadline: 0,
        categories: 0,
        capacity: 0,
        running: false,
        cpus: [0; 4],
    });
static CPUS: [AtomicU64; 4] = [const { AtomicU64::new(u64::MAX) }; 4];
static EXPIRY: PublishedOnce<Callout<()>> = PublishedOnce::new();

pub(super) fn initialize() -> core::result::Result<(), trace::Error> {
    let timer = Callout::try_new((), expire).map_err(|_| trace::Error::Unavailable)?;
    EXPIRY
        .publish(timer)
        .map_err(|_| trace::Error::AlreadyInitialized)
}

pub(super) fn selected(cpu: usize) -> bool {
    CPUS.get(cpu / 64)
        .is_some_and(|word| word.load(Ordering::Relaxed) & (1 << (cpu % 64)) != 0)
}

pub(crate) fn start(bytes: &[u8; 56]) -> Result<u64> {
    let categories = word(bytes, 0);
    let duration = word(bytes, 8);
    let capacity = word(bytes, 16);
    let online = crate::cpu::online_cpu_count();
    let mut cpus = core::array::from_fn(|i| word(bytes, 24 + i * 8));
    if categories == 0
        || categories & !127 != 0
        || !(1..=300_000).contains(&duration)
        || !(64..=trace::CAPACITY as u64).contains(&capacity)
        || online > 256
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    if cpus == [0; 4] {
        for cpu in 0..online {
            cpus[cpu / 64] |= 1 << (cpu % 64);
        }
    }
    for cpu in online..256 {
        if cpus[cpu / 64] & (1 << (cpu % 64)) != 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
    }
    CONTROL.with(|capture| {
        if capture.running {
            return Err(HYPER_NATIVE_STATUS_BUSY);
        }
        let started = time::monotonic_nanoseconds().map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        let deadline = started
            .checked_add(duration * 1_000_000)
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let timer = EXPIRY.get().ok_or(HYPER_NATIVE_STATUS_NOT_SUPPORTED)?;
        let ticks = time::deadline_from_monotonic_nanoseconds(deadline)
            .map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        // The callout checks the current deadline under CONTROL, so an already
        // claimed callback cannot stop a newer capture after a reset.
        timer
            .reset(ticks)
            .map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        for (dst, mask) in CPUS.iter().zip(cpus) {
            dst.store(mask, Ordering::Relaxed);
        }
        let session = match trace::start_with_capacity(
            Filter::from_bits(categories as u8),
            capacity as usize,
        ) {
            Ok(session) => session,
            Err(error) => {
                let _ = timer.cancel();
                return Err(status(error));
            }
        };
        *capture = Capture {
            session: Some(session),
            started,
            deadline,
            categories,
            capacity,
            running: true,
            cpus,
        };
        Ok(session.generation())
    })
}

fn finish(capture: &mut Capture) -> Result<()> {
    let session = capture.session.ok_or(HYPER_NATIVE_STATUS_BAD_STATE)?;
    trace::stop(session).map_err(status)?;
    capture.running = false;
    Ok(())
}
pub(crate) fn stop(generation: u64) -> Result<()> {
    CONTROL.with(|capture| {
        session(capture, generation)?;
        let result = finish(capture);
        if result.is_ok()
            && let Some(timer) = EXPIRY.get()
        {
            let _ = timer.cancel();
        }
        result
    })
}
fn expire(_: &(), timer: CalloutRef<'_>, _: TimerEvent) {
    CONTROL.with(|capture| {
        if !capture.running {
            return;
        }
        if time::monotonic_nanoseconds().is_ok_and(|now| now < capture.deadline) {
            return;
        }
        if finish(capture).is_err() {
            // Admission was closed even when a short writer still owns a ring.
            // Retry joining from normal context; never spin on another CPU.
            if let Ok(now) = time::monotonic_nanoseconds()
                && let Ok(ticks) =
                    time::deadline_from_monotonic_nanoseconds(now.saturating_add(1_000_000))
            {
                let _ = timer.reset(ticks);
            }
        }
    });
}
fn session(capture: &Capture, generation: u64) -> Result<Session> {
    capture
        .session
        .filter(|s| s.generation() == generation)
        .ok_or(HYPER_NATIVE_STATUS_BAD_STATE)
}

#[inline(never)]
pub(crate) fn query(generation: u64, cpu: u64, cursor: u64, out: &mut [u8]) -> Result<usize> {
    let size = match cpu {
        u64::MAX => 160,
        x if x == u64::MAX - 1 => 72,
        x if x == u64::MAX - 2 => 80,
        _ => 2656,
    };
    if out.len() != size {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    if cpu == u64::MAX - 2 {
        CONTROL.with(|capture| session(capture, generation))?;
        let (name, koid) = crate::task::scheduler::trace_thread_details(cursor)
            .map_err(|_| HYPER_NATIVE_STATUS_NOT_FOUND)?;
        out.fill(0);
        encode(&[cursor, koid.get()], out);
        text(&mut out[16..80], name.as_str());
        return Ok(80);
    }
    if cpu == u64::MAX - 1 {
        CONTROL.with(|capture| session(capture, generation))?;
        let name = crate::task::scheduler::trace_thread_name(cursor)
            .map_err(|_| HYPER_NATIVE_STATUS_NOT_FOUND)?;
        out.fill(0);
        encode(&[cursor], out);
        text(&mut out[8..72], name.as_str());
        return Ok(72);
    }
    CONTROL.with(|capture| {
        out.fill(0);
        if cpu == u64::MAX {
            if generation != 0 || cursor != 0 {
                return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
            }
            encode(
                &[
                    capture.session.map_or(0, Session::generation),
                    capture.started,
                    capture.deadline,
                    capture.categories,
                    crate::cpu::online_cpu_count() as u64,
                    capture.capacity,
                    trace::CAPACITY as u64,
                    u64::from(capture.running),
                ],
                out,
            );
            encode(&capture.cpus, &mut out[64..96]);
            text(
                &mut out[96..160],
                concat!(
                    env!("HYPER_BUILD_REVISION"),
                    " ",
                    env!("HYPER_BUILD_TIMESTAMP")
                ),
            );
            return Ok(160);
        }
        let session = session(capture, generation)?;
        if cpu >= crate::cpu::online_cpu_count() as u64 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let mut records = [None; 8];
        let batch = trace::read(session, cpu as usize, cursor, &mut records).map_err(status)?;
        encode(
            &[
                batch.count as u64,
                batch.next,
                batch.overwritten,
                u64::from(batch.incomplete),
            ],
            out,
        );
        for (dst, record) in out[32..]
            .chunks_exact_mut(328)
            .zip(records.iter().take(batch.count))
        {
            let record = record.ok_or(HYPER_NATIVE_STATUS_INTERNAL)?;
            encode(
                &[
                    record.sequence,
                    record.timestamp_ns,
                    record.site.category as u64,
                    record.site.fields.len() as u64,
                ],
                dst,
            );
            encode(&record.values, &mut dst[32..64]);
            text(&mut dst[64..128], record.site.name);
            for (dst, field) in dst[128..224].chunks_exact_mut(24).zip(record.site.fields) {
                text(dst, field);
            }
            text(&mut dst[224..320], record.site.file);
            encode(&[record.site.line as u64], &mut dst[320..]);
        }
        Ok(2656)
    })
}
fn status(error: trace::Error) -> HyperNativeStatus {
    match error {
        trace::Error::Busy => HYPER_NATIVE_STATUS_BUSY,
        trace::Error::Unavailable => HYPER_NATIVE_STATUS_NOT_SUPPORTED,
        trace::Error::Exhausted => HYPER_NATIVE_STATUS_RESOURCE_LIMIT,
        trace::Error::InvalidSession | trace::Error::NotRecording => HYPER_NATIVE_STATUS_BAD_STATE,
        _ => HYPER_NATIVE_STATUS_INVALID_ARGUMENT,
    }
}
fn word(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}
fn encode(words: &[u64], output: &mut [u8]) {
    for (word, dst) in words.iter().zip(output.chunks_exact_mut(8)) {
        dst.copy_from_slice(&word.to_le_bytes());
    }
}
fn text(output: &mut [u8], value: &str) {
    let mut len = value.len().min(output.len().saturating_sub(1));
    while !value.is_char_boundary(len) {
        len -= 1;
    }
    output[..len].copy_from_slice(&value.as_bytes()[..len]);
}
