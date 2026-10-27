// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Console ownership, filtering, and fatal-output access.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;

use hyper::hal::console::{Console, ConsoleWriter};
use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicUsize, Ordering};
use {
    crate::diagnostics::log::storage::DrainBarrierError,
    crate::diagnostics::log::storage::DrainBarrierRegistration,
    crate::diagnostics::log::storage::DrainBarrierSet,
    crate::diagnostics::log::storage::DrainBarrierStatus,
    crate::diagnostics::log::storage::DrainBarrierToken,
    crate::diagnostics::log::storage::EmergencyQuiescence,
    crate::diagnostics::log::storage::EmergencyWriteGate, crate::diagnostics::log::storage::Level,
    crate::diagnostics::log::storage::Record, crate::diagnostics::log::storage::RecordFlags,
    crate::diagnostics::log::storage::RuntimeByteAccess,
};
use {
    hyper_drivers::serial::console::ConsoleDevice,
    hyper_drivers::serial::console::EmergencyConsoleHandle,
};

type KernelSpinLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

const CONSOLE_LOGLEVEL_DEFAULT: Level = configured_console_level();
const EMERGENCY_QUIESCENCE_POLLS: usize = 4096;
const EMERGENCY_UART_ATTEMPTS: usize = 4096;

struct ConsoleState {
    device: Option<ConsoleDevice>,
    next_sequence: u64,
    maximum_level: Level,
    barriers: Vec<Option<Box<FlushNode>>>,
    active_barriers: usize,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            device: None,
            next_sequence: 0,
            maximum_level: CONSOLE_LOGLEVEL_DEFAULT,
            barriers: Vec::new(),
            active_barriers: 0,
        }
    }
}

static CONSOLE: KernelSpinLock<ConsoleState> = KernelSpinLock::new(ConsoleState::new());
static EMERGENCY_CONSOLE: AtomicUsize = AtomicUsize::new(0);
static EMERGENCY_CONSOLE_METADATA: AtomicUsize = AtomicUsize::new(0);
static EMERGENCY_CONSOLE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static WRITE_GATE: EmergencyWriteGate = EmergencyWriteGate::new();

pub fn install(console: ConsoleDevice) {
    let emergency = console.emergency_handle();
    CONSOLE.with(|state| {
        publish_emergency_handle(Some(emergency));
        state.device = Some(console);
    });
    super::drain::request();
}

/// Retires the identity-mapped console before runtime promotion.
///
/// This is a one-way boot transition. Failure between retirement and the next
/// [`install`] remains diagnosable through the log ring, but must not access a
/// virtual address whose bootstrap mapping no longer exists.
pub(crate) fn retire_bootstrap() {
    super::drain::flush_boot();
    CONSOLE.with(|state| {
        publish_emergency_handle(None);
        state.device = None;
    });
}

/// Publishes both machine words as one sequence-validated snapshot.
///
/// `CONSOLE` serializes writers. Sequential consistency is intentional here:
/// installation is a boot-only slow path, while a fatal reader must never
/// combine the virtual base from one mapping with another driver's metadata.
fn publish_emergency_handle(handle: Option<EmergencyConsoleHandle>) {
    // `CONSOLE` serializes every publisher, so this writer does not need an
    // atomic read-modify-write. Avoiding one also keeps early-console
    // publication independent of outlined atomic RMW helpers.
    let sequence = EMERGENCY_CONSOLE_SEQUENCE.load(Ordering::SeqCst);
    EMERGENCY_CONSOLE_SEQUENCE.store(sequence.wrapping_add(1), Ordering::SeqCst);
    let handle = match handle {
        Some(handle) => handle,
        None => EmergencyConsoleHandle {
            base: 0,
            metadata: 0,
        },
    };
    EMERGENCY_CONSOLE.store(handle.base, Ordering::SeqCst);
    EMERGENCY_CONSOLE_METADATA.store(handle.metadata, Ordering::SeqCst);
    EMERGENCY_CONSOLE_SEQUENCE.store(sequence.wrapping_add(2), Ordering::SeqCst);
}

/// Changes only console filtering; every severity remains in the ring.
pub fn set_loglevel(level: Level) {
    CONSOLE.with(|state| state.maximum_level = level);
    super::drain::request();
}

pub fn loglevel() -> Level {
    CONSOLE.with(|state| state.maximum_level)
}

/// Stops normal ring draining once fatal diagnostics switch to direct output.
///
/// Emergency records remain retained for post-mortem readers, but allowing an
/// unrelated CPU to drain them would print every fatal line a second time.
pub(super) fn enter_emergency_mode() -> EmergencyQuiescence {
    // A normal transaction is one nonblocking status read plus, at most, one
    // byte write, so a remote owner should quiesce well inside this fixed
    // bound. A timeout fails closed: the retained crash record remains
    // available, but direct UART output stays disabled rather than racing a
    // possibly stalled MMIO transaction.
    let current_cpu = crate::cpu::current_index()
        .map(|cpu| cpu.get())
        .unwrap_or(usize::MAX);
    WRITE_GATE.retire_normal_writer(current_cpu, EMERGENCY_QUIESCENCE_POLLS)
}

/// Writes a best-effort fatal message without waiting for kernel log locks.
pub(super) fn emergency_write(prefix: &[u8], message: &[u8]) {
    let Some(device) = emergency_device() else {
        return;
    };
    let mut attempts = EMERGENCY_UART_ATTEMPTS;
    emergency_write_bytes(&device, prefix, &mut attempts);
    emergency_write_bytes(&device, message, &mut attempts);
    if !message.ends_with(b"\n") {
        emergency_write_bytes(&device, b"\n", &mut attempts);
    }
}

#[cfg(feature = "crash-console")]
pub(super) fn emergency_available() -> bool {
    emergency_device().is_some()
}

#[cfg(feature = "crash-console")]
pub(super) fn emergency_write_raw(bytes: &[u8]) {
    if let Some(device) = emergency_device() {
        let mut attempts = EMERGENCY_UART_ATTEMPTS;
        emergency_write_bytes(&device, bytes, &mut attempts);
    }
}

#[cfg(feature = "crash-console")]
pub(super) fn emergency_read_raw() -> Option<u8> {
    emergency_device().and_then(|device| device.try_read_byte())
}

fn emergency_device() -> Option<ConsoleDevice> {
    if !WRITE_GATE.emergency_enabled() {
        return None;
    }
    let sequence = EMERGENCY_CONSOLE_SEQUENCE.load(Ordering::SeqCst);
    if sequence & 1 != 0 {
        // A synchronous failure may interrupt the installing CPU. Do not spin
        // waiting for a writer that cannot resume on this execution path.
        return None;
    }
    let handle = EmergencyConsoleHandle {
        base: EMERGENCY_CONSOLE.load(Ordering::SeqCst),
        metadata: EMERGENCY_CONSOLE_METADATA.load(Ordering::SeqCst),
    };
    if EMERGENCY_CONSOLE_SEQUENCE.load(Ordering::SeqCst) != sequence {
        return None;
    }
    // SAFETY: install publishes only a currently live bootstrap or permanent
    // mapping, and retirement clears the snapshot before invalidating the
    // bootstrap address. Crash handling is the sole lock-free consumer.
    unsafe { ConsoleDevice::from_emergency_handle(handle, crate::hal::platform::port_io()) }
}

#[derive(Clone, Copy)]
pub(super) struct OutputSnapshot {
    pub(super) device: ConsoleDevice,
    pub(super) next_sequence: u64,
    pub(super) maximum_level: Level,
}

pub(super) fn output_snapshot() -> Option<OutputSnapshot> {
    if WRITE_GATE.is_retired() {
        return None;
    }
    CONSOLE.with(|state| {
        state.device.map(|device| OutputSnapshot {
            device,
            next_sequence: state.next_sequence,
            maximum_level: state.maximum_level,
        })
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuntimeByteWrite {
    Accepted,
    WouldBlock,
    Retired,
}

/// Attempts one runtime byte while excluding emergency ownership transition.
pub(super) fn try_write_runtime_byte(device: ConsoleDevice, byte: u8) -> RuntimeByteWrite {
    // The gate records a CPU owner, so a byte transaction must not be
    // preempted or migrated between identity capture and permit release.
    // Release the CPU-owned permit before another thread can run on this CPU.
    // SAFETY: This scope contains only atomic operations and nonblocking MMIO;
    // the guard is dropped locally after the byte permit, with no scheduling.
    let _mask = unsafe { hyper::sync::InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    let Some(cpu) = crate::cpu::current_index() else {
        return RuntimeByteWrite::WouldBlock;
    };
    match WRITE_GATE.try_begin_normal_byte(cpu.get()) {
        RuntimeByteAccess::Acquired(_permit) => {
            if device.try_write_byte(byte) {
                RuntimeByteWrite::Accepted
            } else {
                RuntimeByteWrite::WouldBlock
            }
        }
        RuntimeByteAccess::Busy => RuntimeByteWrite::WouldBlock,
        RuntimeByteAccess::Retired => RuntimeByteWrite::Retired,
    }
}

/// Writes until the fixed attempt budget is consumed, without newline translation.
fn emergency_write_bytes(console: &ConsoleDevice, bytes: &[u8], attempts: &mut usize) {
    for &byte in bytes {
        if !emergency_write_byte(console, byte, attempts) {
            return;
        }
    }
}

fn emergency_write_byte(console: &ConsoleDevice, byte: u8, attempts: &mut usize) -> bool {
    while *attempts != 0 {
        *attempts -= 1;
        if console.try_write_byte(byte) {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

pub(super) fn advance(sequence: u64) -> Result<(), ConsoleProgressError> {
    CONSOLE.with(|state| {
        if state.next_sequence < sequence {
            state.next_sequence = sequence;
            if state.active_barriers != 0 {
                for node in state.barriers.iter_mut().flatten() {
                    node.barrier.advance(sequence);
                }
            }
        }
        wake_completed_barriers(state).map_err(ConsoleProgressError::Scheduler)
    })
}

pub(super) fn advance_overrun(sequence: u64, missed: u64) -> Result<(), ConsoleProgressError> {
    CONSOLE.with(|state| {
        if state.next_sequence < sequence {
            if state.active_barriers != 0 {
                for node in state.barriers.iter_mut().flatten() {
                    node.barrier
                        .advance_overrun(state.next_sequence, sequence, missed)
                        .map_err(ConsoleProgressError::Barrier)?;
                }
            }
            state.next_sequence = sequence;
        }
        wake_completed_barriers(state).map_err(ConsoleProgressError::Scheduler)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConsoleProgressError {
    Barrier(DrainBarrierError),
    Scheduler(crate::task::scheduler::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConsoleFlushOutcome {
    Drained,
    Overrun { missed: u64 },
    NoConsole,
    Emergency,
}

pub(super) enum FlushBarrierRegistration {
    Complete(ConsoleFlushOutcome),
    Pending(FlushBarrier),
}

pub(crate) struct FlushBarrier {
    slot: Option<usize>,
}

// Boxed wait queues stay at the same address across directory growth. Each
// registration owns its allocation until unlinking under CONSOLE; destruction
// happens after dropping that lock, including interrupted/cancelled waits.
struct FlushNode {
    barrier: DrainBarrierSet<1>,
    token: Option<DrainBarrierToken>,
    waiters: crate::task::WaitQueue,
}

/// Registers a finite log-ring watermark while the console cursor is stable.
///
/// Lock order is permanently `CONSOLE -> LOG_RING`. Producers release
/// `LOG_RING` before requesting a drain, and the worker releases it before
/// advancing `CONSOLE`, so the reverse nested order does not exist.
pub(super) fn register_flush_barrier() -> Result<FlushBarrierRegistration, DrainBarrierError> {
    register_barrier(false)
}

fn register_barrier(force_pending: bool) -> Result<FlushBarrierRegistration, DrainBarrierError> {
    let mut node = Some(
        hyper::mm::try_box(FlushNode {
            barrier: DrainBarrierSet::new(),
            token: None,
            waiters: crate::task::WaitQueue::new(),
        })
        .map_err(|_| DrainBarrierError::NoFreeSlot)?,
    );
    let mut prepared = Vec::new();
    loop {
        let result = CONSOLE.with(|state| {
            if !force_pending {
                if WRITE_GATE.is_retired() {
                    return Ok(Ok(FlushBarrierRegistration::Complete(
                        ConsoleFlushOutcome::Emergency,
                    )));
                }
                if state.device.is_none() {
                    return Ok(Ok(FlushBarrierRegistration::Complete(
                        ConsoleFlushOutcome::NoConsole,
                    )));
                }
            }
            if prepared.len() > state.barriers.len() {
                for (old, new) in state.barriers.iter_mut().zip(prepared.iter_mut()) {
                    core::mem::swap(old, new);
                }
                core::mem::swap(&mut state.barriers, &mut prepared);
            }
            let Some(slot) = state.barriers.iter().position(Option::is_none) else {
                return Err(state.barriers.len());
            };
            let target = if force_pending {
                u64::MAX
            } else {
                super::statistics().next_sequence
            };
            let candidate = node
                .as_mut()
                .unwrap_or_else(|| barrier_invariant("missing flush allocation"));
            match candidate.barrier.register(state.next_sequence, target) {
                Ok(DrainBarrierRegistration::Complete) => Ok(Ok(
                    FlushBarrierRegistration::Complete(ConsoleFlushOutcome::Drained),
                )),
                Ok(DrainBarrierRegistration::Pending(token)) => {
                    candidate.token = Some(token);
                    state.barriers[slot] = node.take();
                    state.active_barriers += 1;
                    Ok(Ok(FlushBarrierRegistration::Pending(FlushBarrier {
                        slot: Some(slot),
                    })))
                }
                Err(error) => Ok(Err(error)),
            }
        });
        drop(core::mem::take(&mut prepared));
        match result {
            Ok(result) => return result,
            Err(length) => {
                let length = length
                    .checked_mul(2)
                    .ok_or(DrainBarrierError::NoFreeSlot)?
                    .max(4);
                prepared
                    .try_reserve_exact(length)
                    .map_err(|_| DrainBarrierError::NoFreeSlot)?;
                prepared.resize_with(length, || None);
            }
        }
    }
}

pub(super) fn wait_for_drain(
    mut barrier: FlushBarrier,
) -> Result<ConsoleFlushOutcome, crate::sync::Error> {
    use crate::task::scheduler;
    use {crate::task::WaitMobility, crate::task::WaitOutcome};

    scheduler::ensure_sleepable()?;
    loop {
        let slot = barrier.slot_or_invariant();
        // SAFETY: The retained IRQ mask is consumed by the park transition or
        // dropped before this function resumes ordinary Thread execution.
        let (prepared, interrupt_mask) = unsafe {
            CONSOLE.with_mask_retained(|state| {
                let status = barrier_status_or_invariant(state, slot);
                match status {
                    DrainBarrierStatus::Pending => {
                        let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
                        scheduler::prepare_registered_park_locked(
                            &flush_node(state, slot).waiters,
                            registration,
                        )
                        .map(Some)
                    }
                    _ => Ok(None),
                }
            })
        };
        let Some(prepared) = prepared? else {
            drop(interrupt_mask);
            let status = CONSOLE.with(|state| barrier_status_or_invariant(state, slot));
            let outcome = match status {
                DrainBarrierStatus::Drained => ConsoleFlushOutcome::Drained,
                DrainBarrierStatus::Overrun { missed } => ConsoleFlushOutcome::Overrun { missed },
                DrainBarrierStatus::Pending => continue,
            };
            barrier.release();
            return Ok(outcome);
        };
        let outcome = prepared.retain_mask(interrupt_mask).complete();
        if outcome != WaitOutcome::Notified {
            return Err(crate::sync::Error::WaitInterrupted(outcome));
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn register_pending_flush_barrier_for_test() -> Result<FlushBarrier, DrainBarrierError> {
    match register_barrier(true)? {
        FlushBarrierRegistration::Pending(barrier) => Ok(barrier),
        FlushBarrierRegistration::Complete(_) => {
            barrier_invariant("test flush completed during registration")
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn wait_pending_flush_barrier_for_test(
    barrier: FlushBarrier,
) -> Result<ConsoleFlushOutcome, crate::sync::Error> {
    wait_for_drain(barrier)
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn flush_barrier_waiter_count_for_test(
    slot: usize,
) -> Result<usize, crate::task::scheduler::Error> {
    CONSOLE.with(
        |state| match state.barriers.get(slot).and_then(Option::as_ref) {
            Some(node) => node.waiters.len(),
            None => Err(crate::task::scheduler::Error::InvalidWaitRegistration),
        },
    )
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn cancel_flush_barrier_waiter_for_test(
    slot: usize,
    thread: crate::task::thread::ThreadId,
) -> Result<bool, crate::task::scheduler::Error> {
    CONSOLE.with(
        |state| match state.barriers.get(slot).and_then(Option::as_ref) {
            Some(node) => node.waiters.cancel(thread),
            None => Err(crate::task::scheduler::Error::InvalidWaitRegistration),
        },
    )
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn active_flush_barrier_count_for_test() -> usize {
    CONSOLE.with(|state| state.active_barriers)
}

impl FlushBarrier {
    fn slot_or_invariant(&self) -> usize {
        self.slot
            .unwrap_or_else(|| barrier_invariant("flush barrier consumed twice"))
    }

    fn release(&mut self) {
        let Some(slot) = self.slot.take() else {
            return;
        };
        let retired = CONSOLE.with(|state| {
            let retired = state.barriers[slot].take();
            if retired.is_some() {
                state.active_barriers -= 1;
            }
            retired
        });
        if retired.is_none() {
            barrier_invariant("flush barrier released twice");
        }
        drop(retired);
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(crate) fn slot_for_test(&self) -> usize {
        self.slot_or_invariant()
    }
}

impl Drop for FlushBarrier {
    fn drop(&mut self) {
        self.release();
    }
}

fn flush_node(state: &ConsoleState, slot: usize) -> &FlushNode {
    state
        .barriers
        .get(slot)
        .and_then(Option::as_deref)
        .unwrap_or_else(|| barrier_invariant("stale flush barrier"))
}

fn barrier_status_or_invariant(state: &ConsoleState, slot: usize) -> DrainBarrierStatus {
    let node = flush_node(state, slot);
    let token = node
        .token
        .unwrap_or_else(|| barrier_invariant("missing flush token"));
    node.barrier
        .status(token)
        .unwrap_or_else(|_| barrier_invariant("invalid flush token"))
}

fn wake_completed_barriers(state: &mut ConsoleState) -> Result<(), crate::task::scheduler::Error> {
    if state.active_barriers == 0 || !crate::task::is_ready() {
        return Ok(());
    }
    for node in state.barriers.iter_mut().flatten() {
        if node.barrier.take_completion_notification(0) {
            let _ = crate::task::scheduler::wake_all(&node.waiters)?;
        }
    }
    Ok(())
}

fn barrier_invariant(message: &str) -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: {message}"))
}

pub(super) fn write_record(console: &dyn Console, record: Record, message: &[u8]) {
    let mut writer = ConsoleWriter(console);
    let _ = write!(
        writer,
        "<{}>[{}] ",
        record.level as u8,
        crate::diagnostics::log::storage::Timestamp::from_microseconds(
            record.timestamp_microseconds
        )
    );
    console.write_bytes(message);
    if record.flags.contains(RecordFlags::TRUNCATED) || record.copied != record.length {
        console.write_bytes(b" [truncated]");
    }
    if !message.ends_with(b"\n") {
        console.write_bytes(b"\n");
    }
}

pub(super) fn write_overrun(console: &ConsoleDevice, missed: u64) {
    let mut writer = ConsoleWriter(console);
    let _ = writeln!(
        writer,
        "<4>[{}] {missed} record(s) lost",
        super::timestamp_now()
    );
}

pub(super) fn write_ring_failure(console: &ConsoleDevice, error: super::Error) {
    let mut writer = ConsoleWriter(console);
    let _ = writeln!(
        writer,
        "<2>[{}] ring read failure: {error:?}",
        super::timestamp_now()
    );
}

pub(super) fn write_console_tx(console: &ConsoleDevice, bytes: &[u8]) {
    console.write_bytes(bytes);
}

pub(super) fn write_console_tx_overflow(console: &ConsoleDevice, dropped: u64) {
    let mut writer = ConsoleWriter(console);
    let _ = writeln!(
        writer,
        "<4>[{}] {dropped} Console TX byte(s) lost",
        super::timestamp_now()
    );
}

const fn configured_console_level() -> Level {
    match Level::from_u8(hyper::config::CONSOLE_LOGLEVEL_DEFAULT as u8) {
        Some(level) => level,
        None => Level::Info,
    }
}
