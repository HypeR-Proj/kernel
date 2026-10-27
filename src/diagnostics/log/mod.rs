// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel log production, retention, and reader API.

pub mod storage;

use core::fmt::{self, Write};

use hyper::sync::InterruptSpinLock;
use {
    crate::diagnostics::log::storage::AppendError,
    crate::diagnostics::log::storage::EmergencyQuiescence, crate::diagnostics::log::storage::Level,
    crate::diagnostics::log::storage::ReadError, crate::diagnostics::log::storage::ReadResult,
    crate::diagnostics::log::storage::RecordFlags, crate::diagnostics::log::storage::RingBuffer,
    crate::diagnostics::log::storage::Timestamp,
};

pub(crate) mod console;
mod drain;

pub(crate) use drain::InitializationError;
pub use drain::{FlushError, FlushOutcome};

type KernelSpinLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

const LOG_BUFFER_SIZE: usize = 1usize << hyper::config::LOG_BUF_SHIFT as usize;
const LOG_LINE_MAX: usize = hyper::config::LOG_LINE_MAX as usize;
const LOG_COMPILE_LEVEL: Level = configured_compile_level();

struct FormatBuffer {
    bytes: [u8; LOG_LINE_MAX],
    length: usize,
    truncated: bool,
}

impl FormatBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; LOG_LINE_MAX],
            length: 0,
            truncated: false,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl fmt::Write for FormatBuffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let available = LOG_LINE_MAX - self.length;
        let copied = available.min(text.len());
        self.bytes[self.length..self.length + copied].copy_from_slice(&text.as_bytes()[..copied]);
        self.length += copied;
        self.truncated |= copied != text.len();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Append(AppendError),
    Read(ReadError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Statistics {
    pub capacity: usize,
    pub dropped: u64,
    pub next_sequence: u64,
}

static LOG_RING: KernelSpinLock<RingBuffer<LOG_BUFFER_SIZE>> =
    KernelSpinLock::new(RingBuffer::new());

pub(super) fn timestamp_now() -> Timestamp {
    Timestamp::from_microseconds(crate::time::monotonic_microseconds())
}

pub fn log(level: Level, arguments: fmt::Arguments<'_>) -> Result<(), Error> {
    if !compiled_in(level) {
        return Ok(());
    }
    let timestamp = crate::time::monotonic_microseconds();
    let mut formatted = FormatBuffer::new();
    let _ = formatted.write_fmt(arguments);
    let flags = if formatted.truncated {
        RecordFlags::TRUNCATED
    } else {
        RecordFlags::NONE
    };
    LOG_RING
        .with(|ring| ring.append(level, timestamp, formatted.as_slice(), flags))
        .map_err(Error::Append)?;
    drain::request();
    Ok(())
}

/// Returns whether records at `level` are present in this kernel image.
///
/// Log macros use this before constructing `fmt::Arguments`, so disabled
/// callsites have neither argument-evaluation nor ring-buffer side effects.
#[doc(hidden)]
pub const fn compiled_in(level: Level) -> bool {
    (level as u8) <= (LOG_COMPILE_LEVEL as u8)
}

/// Compatibility path for existing unclassified output.
pub fn write(arguments: fmt::Arguments<'_>) {
    let _ = log(Level::Info, arguments);
}

/// Copies one record without advancing any global reader cursor.
pub fn read(sequence: u64, output: &mut [u8]) -> Result<ReadResult, Error> {
    LOG_RING
        .with(|ring| ring.read(sequence, output))
        .map_err(Error::Read)
}

pub fn statistics() -> Statistics {
    LOG_RING.with(|ring| Statistics {
        capacity: ring.capacity(),
        dropped: ring.dropped(),
        next_sequence: ring.next_sequence(),
    })
}

pub fn set_console_loglevel(level: Level) {
    console::set_loglevel(level);
}

pub fn console_loglevel() -> Level {
    console::loglevel()
}

/// Requests normal-console progress without waiting for the UART.
pub fn request_flush() {
    drain::request();
}

/// Waits for the finite record watermark captured when this call begins.
pub fn flush_sync() -> Result<FlushOutcome, FlushError> {
    drain::flush_sync()
}

/// Starts deferred normal-console draining after scheduler and timer setup.
pub(crate) fn initialize() -> Result<(), InitializationError> {
    drain::initialize()
}

/// Services one producer prompt after interrupt-registry dispatch is complete.
pub(crate) fn service_irq_prompt() {
    drain::service_irq_prompt();
}

/// Queues opaque Console TX bytes for the sole normal UART writer.
pub(crate) fn try_write_console_tx(bytes: &[u8]) -> usize {
    drain::try_enqueue_console_tx(bytes)
}

/// Returns the permanent scheduler population owned by runtime logging.
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) const fn permanent_worker_count_for_test() -> usize {
    1
}

/// Reports the state of the logging backend at the end of kernel startup.
pub(crate) fn report_startup_state() {
    let statistics = statistics();
    crate::pr_info!("HypeR: kernel log ring: {} bytes", statistics.capacity);
}

/// Emits a fatal diagnostic without waiting for a potentially interrupted
/// logging lock. Retention in the ring is best-effort; direct console output is
/// attempted independently so a lock failure cannot stall the fail-stop path.
pub fn emergency(arguments: fmt::Arguments<'_>) {
    let timestamp = crate::time::monotonic_microseconds();
    let mut formatted = FormatBuffer::new();
    let _ = formatted.write_fmt(arguments);
    let flags = if formatted.truncated {
        RecordFlags::TRUNCATED
    } else {
        RecordFlags::NONE
    };
    let _ = LOG_RING.try_with(|ring| {
        let _ = ring.append(Level::Emergency, timestamp, formatted.as_slice(), flags);
    });
    let mut prefix = FormatBuffer::new();
    let _ = write!(prefix, "<0>[{}] ", Timestamp::from_microseconds(timestamp));
    console::emergency_write(prefix.as_slice(), formatted.as_slice());
}

/// Transfers console ownership to the lock-free fatal-output path.
pub(crate) fn enter_emergency_mode() {
    drain::enter_emergency_mode();
    match console::enter_emergency_mode() {
        EmergencyQuiescence::Quiescent => {}
        EmergencyQuiescence::LocalOwnerAbandoned => emergency(format_args!(
            "emergency console recovered by abandoning an interrupted local writer"
        )),
        EmergencyQuiescence::RemoteOwnerTimedOut => emergency(format_args!(
            "emergency console unavailable: remote normal writer did not quiesce"
        )),
    }
}

#[cfg(feature = "crash-console")]
pub(crate) fn crash_console_available() -> bool {
    console::emergency_available()
}

#[cfg(feature = "crash-console")]
pub(crate) fn crash_console_write(bytes: &[u8]) {
    console::emergency_write_raw(bytes);
}

#[cfg(feature = "crash-console")]
pub(crate) fn crash_console_read() -> Option<u8> {
    console::emergency_read_raw()
}

#[macro_export]
macro_rules! printk {
    ($level:expr, $($argument:tt)*) => {{
        let level = $level;
        if $crate::diagnostics::log::compiled_in(level) {
            let _ = $crate::diagnostics::log::log(level, core::format_args!($($argument)*));
        }
    }};
}

/// Emits one statically leveled record only when that level is compiled in.
///
/// The disabled branch retains format checking and marks referenced values as
/// used, while its constant-false body is removed before code generation.
#[doc(hidden)]
#[macro_export]
macro_rules! __printk_static {
    ($configuration:meta, $level:expr, $($argument:tt)*) => {{
        #[cfg($configuration)]
        {
            $crate::printk!($level, $($argument)*);
        }
        #[cfg(not($configuration))]
        {
            if false {
                let _ = core::format_args!($($argument)*);
            }
        }
    }};
}

#[macro_export]
macro_rules! pr_emerg {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_emergency, $crate::diagnostics::log::storage::Level::Emergency, $($argument)*) };
}

#[macro_export]
macro_rules! pr_alert {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_alert, $crate::diagnostics::log::storage::Level::Alert, $($argument)*) };
}

#[macro_export]
macro_rules! pr_crit {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_critical, $crate::diagnostics::log::storage::Level::Critical, $($argument)*) };
}

#[macro_export]
macro_rules! pr_err {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_error, $crate::diagnostics::log::storage::Level::Error, $($argument)*) };
}

#[macro_export]
macro_rules! pr_warn {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_warning, $crate::diagnostics::log::storage::Level::Warning, $($argument)*) };
}

#[macro_export]
macro_rules! pr_notice {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_notice, $crate::diagnostics::log::storage::Level::Notice, $($argument)*) };
}

#[macro_export]
macro_rules! pr_info {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_info, $crate::diagnostics::log::storage::Level::Info, $($argument)*) };
}

#[macro_export]
macro_rules! pr_debug {
    ($($argument:tt)*) => { $crate::__printk_static!(hyper_log_compile_debug, $crate::diagnostics::log::storage::Level::Debug, $($argument)*) };
}

const fn configured_compile_level() -> Level {
    match Level::from_u8(hyper::config::LOG_COMPILE_LEVEL as u8) {
        Some(level) => level,
        None => Level::Info,
    }
}
