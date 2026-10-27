// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-open sequential prediction, independent of filesystem and scheduling.

use hyper::mm::PAGE_SIZE;

use super::FileIdentity;

pub(crate) const MAX_WINDOW: usize = 512 * 1024;
const INITIAL_WINDOW: u64 = 16 * 1024;
const STREAM_READ: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Range {
    pub(crate) offset: u64,
    pub(crate) length: usize,
}

/// Predictions never determine the result of a demand read. A rejected job
/// merely loses an opportunity; a subsequent demand always uses normal I/O.
pub(crate) struct SequentialReader {
    identity: Option<FileIdentity>,
    next: u64,
    issued_end: u64,
    window: u64,
    cold_stream: bool,
}

impl SequentialReader {
    pub(crate) const fn new() -> Self {
        Self {
            identity: None,
            next: 0,
            issued_end: 0,
            window: INITIAL_WINDOW,
            cold_stream: false,
        }
    }

    /// Observe successful demand bytes after releasing the file-content gate.
    /// Hot reopened files do not start speculative I/O. Once a cold sequential
    /// stream is established, consuming its prefetched pages advances the
    /// prediction too. A seek or content revision change resets that stream.
    pub(crate) fn observe(
        &mut self,
        identity: FileIdentity,
        file_length: u64,
        offset: u64,
        completed: usize,
        had_miss: bool,
    ) -> Option<Range> {
        let count = u64::try_from(completed).ok()?;
        let end = offset.checked_add(count)?;
        if count == 0 || end > file_length {
            return None;
        }
        let consecutive = self.identity == Some(identity) && self.next == offset;
        if !consecutive {
            self.identity = Some(identity);
            self.issued_end = end;
            self.window = INITIAL_WINDOW;
            self.cold_stream = false;
        }
        self.next = end;
        self.cold_stream |= had_miss;
        if !self.cold_stream || (!consecutive && count < STREAM_READ) {
            return None;
        }
        // Refill when half the previous window has been consumed. A failed or
        // discarded prediction cannot suppress demand and is eventually retried
        // as consumption crosses this finite frontier.
        if end.saturating_add(self.window / 2) < self.issued_end {
            return None;
        }
        self.window = self
            .window
            .max(count)
            .saturating_mul(if consecutive { 2 } else { 1 })
            .min(MAX_WINDOW as u64);
        // Begin on the page containing the frontier: unaligned demand must not
        // leave that page permanently uncacheable. Never reread earlier pages.
        let frontier = end.max(self.issued_end);
        let start = frontier - frontier % PAGE_SIZE;
        let limit = start.saturating_add(self.window).min(file_length);
        if limit <= end || start >= file_length {
            return None;
        }
        self.issued_end = limit;
        Some(Range {
            offset: start,
            length: (limit - start) as usize,
        })
    }
}
