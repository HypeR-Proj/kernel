// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deferred normal-console ownership and bounded drain policy.

use core::fmt::Write;

use hyper::sync::InterruptSpinLock;
use hyper::sync::atomic::{AtomicU8, Ordering};
use hyper_drivers::serial::console::ConsoleDevice;
use {
    crate::diagnostics::log::storage::ByteRing, crate::diagnostics::log::storage::DeferredDrain,
    crate::diagnostics::log::storage::DrainBarrierError,
    crate::diagnostics::log::storage::DrainDisposition,
    crate::diagnostics::log::storage::OutputBuffer, crate::diagnostics::log::storage::OutputError,
    crate::diagnostics::log::storage::ReadResult, crate::diagnostics::log::storage::Record,
    crate::diagnostics::log::storage::RecordFlags,
};

use crate::sync::Completion;
use crate::task::scheduler;

type ConsoleTxLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

const BOOT: u8 = 0;
const RUNTIME: u8 = 1;
const EMERGENCY: u8 = 2;
const LOG_RECORDS_PER_BATCH: usize = 32;
const CONSOLE_TX_QUEUE_CAPACITY: usize = 4096;
const CONSOLE_TX_FRAME_BYTES: usize = 256;
const LOG_LINE_MAX: usize = hyper::config::LOG_LINE_MAX as usize;
const LOG_OUTPUT_BUFFER_SIZE: usize = LOG_LINE_MAX + 128;
const CONSOLE_TX_OUTPUT_BUFFER_SIZE: usize = CONSOLE_TX_FRAME_BYTES;
const OUTPUT_BUFFER_SIZE: usize = if LOG_OUTPUT_BUFFER_SIZE > CONSOLE_TX_OUTPUT_BUFFER_SIZE {
    LOG_OUTPUT_BUFFER_SIZE
} else {
    CONSOLE_TX_OUTPUT_BUFFER_SIZE
};
const UART_BYTES_PER_BATCH: usize = 256;

static MODE: AtomicU8 = AtomicU8::new(BOOT);
static WORK: DeferredDrain = DeferredDrain::new();
static WAKE: Completion = Completion::new();
static CONSOLE_TX_QUEUE: ConsoleTxLock<ConsoleTxState> =
    InterruptSpinLock::new(ConsoleTxState::new());

struct ConsoleTxState {
    bytes: ByteRing<CONSOLE_TX_QUEUE_CAPACITY>,
    reported_dropped: u64,
}

#[derive(Clone, Copy)]
enum PendingCommit {
    None,
    Log { sequence: u64, missed: u64 },
    ConsoleFrame { count: usize },
    ConsoleOverflow { total_dropped: u64 },
    RingFailure,
}

struct WorkerOutput {
    bytes: OutputBuffer<OUTPUT_BUFFER_SIZE>,
    commit: PendingCommit,
    device: Option<ConsoleDevice>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchOutcome {
    Idle,
    More,
    Backpressured,
    Emergency,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrepareOutcome {
    Prepared,
    MoreKernelRecords,
    Idle,
}

impl WorkerOutput {
    const fn new() -> Self {
        Self {
            bytes: OutputBuffer::new(),
            commit: PendingCommit::None,
            device: None,
        }
    }
}

impl ConsoleTxState {
    const fn new() -> Self {
        Self {
            bytes: ByteRing::new(),
            reported_dropped: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitializationError {
    AlreadyInitialized,
    Scheduler(scheduler::Error),
    WorkerOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Drained,
    Overrun { missed: u64 },
    NoConsole,
    Emergency,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushError {
    NotRuntime,
    Barrier(DrainBarrierError),
    Wait(crate::sync::Error),
}

impl From<scheduler::Error> for InitializationError {
    fn from(error: scheduler::Error) -> Self {
        Self::Scheduler(error)
    }
}

/// Creates the sole normal-runtime UART writer and changes producer ownership.
///
/// The worker is made runnable before Runtime publication. The bootstrap
/// Thread remains current throughout this call, so it cannot observe partially
/// initialized deferred state through a context switch.
pub(super) fn initialize() -> Result<(), InitializationError> {
    if MODE.load(Ordering::Acquire) != BOOT {
        return Err(InitializationError::AlreadyInitialized);
    }
    let worker = scheduler::kthread_create("klogd", worker_entry, 0)?;
    if !WORK.claim_initial_worker() {
        return Err(InitializationError::WorkerOwnership);
    }
    if !scheduler::thread_ready(worker)? {
        return Err(InitializationError::Scheduler(
            scheduler::Error::InvalidThreadState,
        ));
    }
    MODE.compare_exchange(BOOT, RUNTIME, Ordering::Release, Ordering::Acquire)
        .map_err(|_| InitializationError::AlreadyInitialized)?;
    let _ = WORK.request();
    Ok(())
}

/// Publishes normal output without entering scheduler or console-driver code.
pub(super) fn request() {
    let mode = MODE.load(Ordering::Acquire);
    if mode == BOOT {
        drain_boot_synchronously();
        return;
    }
    if mode == RUNTIME && WORK.request() {
        prompt_local_cpu();
    }
}

/// Converts a durable producer request into one scheduler wake at IRQ entry.
///
/// Interrupt registry dispatch has already returned before this seam runs, so
/// Completion cannot nest inside a registered interrupt handler or its lock.
pub(crate) fn service_irq_prompt() {
    if MODE.load(Ordering::Acquire) != RUNTIME
        || !WORK.consume_prompt()
        || !WORK.claim_notification()
    {
        return;
    }
    if let Err(error) = WAKE.complete() {
        crate::diagnostics::crash::fatal(format_args!(
            "HypeR: deferred log wake invariant failed: {error:?}"
        ));
    }
}

/// Waits in schedulable Thread context for records present at call time.
pub(super) fn flush_sync() -> Result<FlushOutcome, FlushError> {
    if MODE.load(Ordering::Acquire) != RUNTIME {
        return if MODE.load(Ordering::Acquire) == EMERGENCY {
            Ok(FlushOutcome::Emergency)
        } else {
            Err(FlushError::NotRuntime)
        };
    }
    scheduler::ensure_sleepable()
        .map_err(crate::sync::Error::from)
        .map_err(FlushError::Wait)?;
    let registration = super::console::register_flush_barrier().map_err(FlushError::Barrier)?;
    let outcome = match registration {
        super::console::FlushBarrierRegistration::Complete(outcome) => outcome,
        super::console::FlushBarrierRegistration::Pending(barrier) => {
            request();
            super::console::wait_for_drain(barrier).map_err(FlushError::Wait)?
        }
    };
    match outcome {
        super::console::ConsoleFlushOutcome::Drained => Ok(FlushOutcome::Drained),
        super::console::ConsoleFlushOutcome::Overrun { missed } => {
            Ok(FlushOutcome::Overrun { missed })
        }
        super::console::ConsoleFlushOutcome::NoConsole => Ok(FlushOutcome::NoConsole),
        super::console::ConsoleFlushOutcome::Emergency => Ok(FlushOutcome::Emergency),
    }
}

/// Sends only a hardware/event prompt; the durable work bit remains authoritative.
fn prompt_local_cpu() {
    match crate::cpu::current_index() {
        Some(cpu) => crate::irq::reschedule::notify(cpu),
        None => crate::hal::cpu::send_event(),
    }
}

/// Enqueues a bounded userspace Console write without interpreting its bytes.
pub(super) fn try_enqueue_console_tx(bytes: &[u8]) -> usize {
    if MODE.load(Ordering::Acquire) != RUNTIME || bytes.is_empty() {
        return 0;
    }
    let accepted = CONSOLE_TX_QUEUE.with(|queue| {
        let was_writable = queue.bytes.remaining_capacity() != 0;
        let accepted = queue.bytes.remaining_capacity().min(bytes.len());
        for &byte in &bytes[..accepted] {
            if !queue.bytes.push(byte) {
                worker_failure("raw enqueue", "reserved queue capacity disappeared")
            }
        }
        let is_writable = queue.bytes.remaining_capacity() != 0;
        if was_writable != is_writable {
            crate::device::console::publish_writable(is_writable);
        }
        accepted
    });
    if accepted != 0 {
        request();
    }
    accepted
}

/// Gives fatal output permanent priority over normal runtime draining.
pub(super) fn enter_emergency_mode() {
    MODE.store(EMERGENCY, Ordering::Release);
}

extern "C" fn worker_entry(_argument: usize) {
    let mut output = WorkerOutput::new();
    loop {
        if MODE.load(Ordering::Acquire) == EMERGENCY {
            scheduler::exit_current();
        }

        WORK.begin_batch();
        match drain_runtime_batch(&mut output) {
            BatchOutcome::Emergency => scheduler::exit_current(),
            BatchOutcome::Backpressured => {
                // Retain the exact pending output but leave the run queue until
                // a later periodic IRQ observes the prompt. A wedged UART can
                // therefore consume at most one bounded attempt per IRQ.
                WORK.defer_until_irq();
                wait_for_worker_prompt();
            }
            BatchOutcome::More => {
                if let Err(error) = scheduler::cond_resched() {
                    worker_failure("reschedule", error)
                }
            }
            BatchOutcome::Idle => match WORK.finish_batch(false) {
                DrainDisposition::Continue => {
                    if let Err(error) = scheduler::cond_resched() {
                        worker_failure("reschedule", error)
                    }
                }
                DrainDisposition::Wait => wait_for_worker_prompt(),
            },
        }
    }
}

fn wait_for_worker_prompt() {
    if let Err(error) = WAKE.wait() {
        worker_failure("wait", error)
    }
}

fn worker_failure(operation: &str, error: impl core::fmt::Debug) -> ! {
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: deferred log worker {operation} failed: {error:?}"
    ))
}

fn drain_boot_synchronously() {
    while drain_boot_log_records() || drain_boot_console_tx() {}
}

fn drain_runtime_batch(output: &mut WorkerOutput) -> BatchOutcome {
    if MODE.load(Ordering::Acquire) == EMERGENCY {
        return BatchOutcome::Emergency;
    }
    let mut accepted = 0;
    while accepted < UART_BYTES_PER_BATCH {
        if output.bytes.is_empty() {
            let Some(snapshot) = super::console::output_snapshot() else {
                return if MODE.load(Ordering::Acquire) == EMERGENCY {
                    BatchOutcome::Emergency
                } else {
                    BatchOutcome::Idle
                };
            };
            match prepare_runtime_output(output, snapshot) {
                PrepareOutcome::Prepared => {}
                PrepareOutcome::MoreKernelRecords => return BatchOutcome::More,
                PrepareOutcome::Idle => {
                    return if MODE.load(Ordering::Acquire) == EMERGENCY {
                        BatchOutcome::Emergency
                    } else {
                        BatchOutcome::Idle
                    };
                }
            }
        }
        let Some(device) = output.device else {
            worker_failure("output device", "missing frame owner")
        };
        let budget = UART_BYTES_PER_BATCH - accepted;
        let mut retired = false;
        let progress =
            output
                .bytes
                .try_write(
                    budget,
                    |byte| match super::console::try_write_runtime_byte(device, byte) {
                        super::console::RuntimeByteWrite::Accepted => true,
                        super::console::RuntimeByteWrite::WouldBlock => false,
                        super::console::RuntimeByteWrite::Retired => {
                            retired = true;
                            false
                        }
                    },
                );
        if retired {
            return BatchOutcome::Emergency;
        }
        accepted += progress.accepted;
        if !progress.complete {
            return if progress.blocked {
                BatchOutcome::Backpressured
            } else {
                BatchOutcome::More
            };
        }
        commit_runtime_output(output);
    }
    BatchOutcome::More
}

fn prepare_runtime_output(
    output: &mut WorkerOutput,
    snapshot: super::console::OutputSnapshot,
) -> PrepareOutcome {
    output.bytes.clear();
    output.commit = PendingCommit::None;
    // Structured kernel diagnostics own the normal Console sink whenever a
    // record is pending. Console TX remains independently buffered and makes
    // progress when the diagnostic source is idle; only emergency output may
    // interrupt an already prepared frame.
    match prepare_log_output(output, snapshot) {
        PrepareOutcome::Prepared => {
            output.device = Some(snapshot.device);
            PrepareOutcome::Prepared
        }
        PrepareOutcome::MoreKernelRecords => PrepareOutcome::MoreKernelRecords,
        PrepareOutcome::Idle if prepare_console_tx_frame(output) => {
            output.device = Some(snapshot.device);
            PrepareOutcome::Prepared
        }
        PrepareOutcome::Idle => PrepareOutcome::Idle,
    }
}

fn prepare_log_output(
    output: &mut WorkerOutput,
    mut snapshot: super::console::OutputSnapshot,
) -> PrepareOutcome {
    let mut message = [0u8; LOG_LINE_MAX];
    for _ in 0..LOG_RECORDS_PER_BATCH {
        match super::read(snapshot.next_sequence, &mut message) {
            Ok(ReadResult::Record(record)) => {
                if record.level > snapshot.maximum_level {
                    publish_log_progress(record.sequence.wrapping_add(1), 0);
                    let Some(next) = super::console::output_snapshot() else {
                        return PrepareOutcome::Idle;
                    };
                    snapshot = next;
                    continue;
                }
                if prepare_record(&mut output.bytes, record, &message[..record.copied]).is_err() {
                    worker_failure("record formatting", OutputError::Full)
                }
                output.commit = PendingCommit::Log {
                    sequence: record.sequence.wrapping_add(1),
                    missed: 0,
                };
                return PrepareOutcome::Prepared;
            }
            Ok(ReadResult::Overrun {
                oldest_sequence,
                missed,
            }) => {
                if prepare_overrun(&mut output.bytes, missed).is_err() {
                    worker_failure("overrun formatting", OutputError::Full)
                }
                output.commit = PendingCommit::Log {
                    sequence: oldest_sequence,
                    missed,
                };
                return PrepareOutcome::Prepared;
            }
            Ok(ReadResult::Empty { .. }) => return PrepareOutcome::Idle,
            Err(error) => {
                if prepare_ring_failure(&mut output.bytes, error).is_err() {
                    worker_failure("ring-failure formatting", OutputError::Full)
                }
                output.commit = PendingCommit::RingFailure;
                return PrepareOutcome::Prepared;
            }
        }
    }
    // A run of filtered records still consumes only a bounded batch. Retain a
    // durable internal request and yield before inspecting Console TX, so a
    // later eligible kernel record cannot be overtaken by userspace output.
    let _ = WORK.request();
    PrepareOutcome::MoreKernelRecords
}

fn prepare_console_tx_frame(output: &mut WorkerOutput) -> bool {
    let mut bytes = [0u8; CONSOLE_TX_FRAME_BYTES];
    let (count, dropped, reported) = CONSOLE_TX_QUEUE.with(|queue| {
        let count = queue.bytes.peek_into(&mut bytes);
        (count, queue.bytes.dropped(), queue.reported_dropped)
    });
    if dropped > reported {
        if prepare_console_tx_overflow(&mut output.bytes, dropped - reported).is_err() {
            worker_failure("Console TX overflow formatting", OutputError::Full)
        }
        output.commit = PendingCommit::ConsoleOverflow {
            total_dropped: dropped,
        };
        return true;
    }
    if count == 0 {
        return false;
    }
    if output.bytes.push_bytes(&bytes[..count]).is_err() {
        worker_failure("Console TX frame preparation", OutputError::Full)
    }
    output.commit = PendingCommit::ConsoleFrame { count };
    true
}

fn commit_runtime_output(output: &mut WorkerOutput) {
    match output.commit {
        PendingCommit::None => worker_failure("output commit", "missing commit owner"),
        PendingCommit::Log { sequence, missed } => publish_log_progress(sequence, missed),
        PendingCommit::ConsoleFrame { count } => CONSOLE_TX_QUEUE.with(|queue| {
            let was_writable = queue.bytes.remaining_capacity() != 0;
            // CONSOLE_TX_QUEUE has one consumer. Producers can only append
            // while the prepared bytes are in flight, so the observed prefix
            // remains stable until this successful physical write commits it.
            if !queue.bytes.discard_front(count) {
                worker_failure("Console TX commit", "prepared prefix disappeared")
            }
            if !was_writable {
                crate::device::console::publish_writable(true);
            }
        }),
        PendingCommit::ConsoleOverflow { total_dropped } => CONSOLE_TX_QUEUE.with(|queue| {
            if queue.reported_dropped < total_dropped {
                queue.reported_dropped = total_dropped;
            }
        }),
        PendingCommit::RingFailure => worker_failure(
            "ring integrity",
            "ring read failed after diagnostic publication",
        ),
    }
    output.bytes.clear();
    output.commit = PendingCommit::None;
    output.device = None;
}

fn publish_log_progress(sequence: u64, missed: u64) {
    let result = if missed == 0 {
        super::console::advance(sequence)
    } else {
        super::console::advance_overrun(sequence, missed)
    };
    if let Err(error) = result {
        worker_failure("record progress publication", error)
    }
}

fn prepare_record(
    output: &mut OutputBuffer<OUTPUT_BUFFER_SIZE>,
    record: Record,
    message: &[u8],
) -> Result<(), OutputError> {
    write!(
        output,
        "<{}>[{}] ",
        record.level as u8,
        crate::diagnostics::log::storage::Timestamp::from_microseconds(
            record.timestamp_microseconds
        )
    )
    .map_err(|_| OutputError::Full)?;
    output.push_bytes(message)?;
    if record.flags.contains(RecordFlags::TRUNCATED) || record.copied != record.length {
        output.push_bytes(b" [truncated]")?;
    }
    if !message.ends_with(b"\n") {
        output.push_bytes(b"\n")?;
    }
    Ok(())
}

fn prepare_overrun(
    output: &mut OutputBuffer<OUTPUT_BUFFER_SIZE>,
    missed: u64,
) -> Result<(), OutputError> {
    write!(
        output,
        "<4>[{}] {missed} record(s) lost",
        super::timestamp_now()
    )
    .map_err(|_| OutputError::Full)?;
    output.push_bytes(b"\n")
}

fn prepare_ring_failure(
    output: &mut OutputBuffer<OUTPUT_BUFFER_SIZE>,
    error: super::Error,
) -> Result<(), OutputError> {
    write!(
        output,
        "<2>[{}] ring read failure: {error:?}",
        super::timestamp_now()
    )
    .map_err(|_| OutputError::Full)?;
    output.push_bytes(b"\n")
}

fn prepare_console_tx_overflow(
    output: &mut OutputBuffer<OUTPUT_BUFFER_SIZE>,
    dropped: u64,
) -> Result<(), OutputError> {
    write!(
        output,
        "<4>[{}] {dropped} Console TX byte(s) lost",
        super::timestamp_now()
    )
    .map_err(|_| OutputError::Full)?;
    output.push_bytes(b"\n")
}

fn drain_boot_log_records() -> bool {
    let mut message = [0u8; LOG_LINE_MAX];
    for _ in 0..LOG_RECORDS_PER_BATCH {
        let Some(output) = super::console::output_snapshot() else {
            return false;
        };
        match super::read(output.next_sequence, &mut message) {
            Ok(ReadResult::Record(record)) => {
                if record.level <= output.maximum_level {
                    super::console::write_record(&output.device, record, &message[..record.copied]);
                }
                publish_log_progress(record.sequence.wrapping_add(1), 0);
            }
            Ok(ReadResult::Overrun {
                oldest_sequence,
                missed,
            }) => {
                super::console::write_overrun(&output.device, missed);
                publish_log_progress(oldest_sequence, missed);
            }
            Ok(ReadResult::Empty { .. }) => return false,
            Err(error) => {
                super::console::write_ring_failure(&output.device, error);
                return false;
            }
        }
    }
    true
}

fn drain_boot_console_tx() -> bool {
    let Some(output) = super::console::output_snapshot() else {
        return false;
    };
    let mut bytes = [0u8; CONSOLE_TX_FRAME_BYTES];
    let (count, more, newly_dropped) = CONSOLE_TX_QUEUE.with(|queue| {
        let count = queue.bytes.pop_into(&mut bytes);
        let more = !queue.bytes.is_empty();
        let dropped = queue.bytes.dropped().saturating_sub(queue.reported_dropped);
        queue.reported_dropped = queue.bytes.dropped();
        (count, more, dropped)
    });
    if newly_dropped != 0 {
        super::console::write_console_tx_overflow(&output.device, newly_dropped);
    }
    if count != 0 {
        super::console::write_console_tx(&output.device, &bytes[..count]);
    }
    more
}

/// Drains retained boot records before retiring an identity-mapped console.
pub(super) fn flush_boot() {
    if MODE.load(Ordering::Acquire) == BOOT {
        drain_boot_synchronously();
    }
}
