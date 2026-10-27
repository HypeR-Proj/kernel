// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Content coherence shared by every open of one backend node.

use core::num::NonZeroU64;

use crate::fs::cache::ContentRevision;
use {crate::sync::Mutex, crate::sync::MutexGuard};

use super::instance::Error;
use super::protection_state::ProtectionState;

/// One content incarnation owns this gate, so aliases and independent opens
/// share it. Cacheable backends keep it in a shared `FileRecord`.
///
/// Acquire it before backend locks and retain it through reads, cache copies,
/// synchronous mutations, and snapshot creation. Reads of the same file are
/// serialized. Cache entries may retain that content record, but never an
/// active node lease: caching a page must not prevent unlink.
pub(super) struct FileContent {
    state: Mutex<ContentState>,
    protections: ProtectionState,
}

struct ContentState {
    revision: NonZeroU64,
    known_length: Option<u64>,
}

pub(super) struct ContentGuard<'a> {
    state: MutexGuard<'a, ContentState>,
    protections: &'a ProtectionState,
}

impl FileContent {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(ContentState {
                revision: NonZeroU64::MIN,
                known_length: None,
            }),
            protections: ProtectionState::new(),
        }
    }

    pub(super) fn lock(&self) -> Result<ContentGuard<'_>, Error> {
        Ok(ContentGuard {
            state: self.state.lock().map_err(Error::Lock)?,
            protections: &self.protections,
        })
    }

    /// Speculative work must not queue ahead of a demand reader or mutation.
    pub(super) fn try_lock(&self) -> Result<Option<ContentGuard<'_>>, Error> {
        Ok(self
            .state
            .try_lock()
            .map_err(Error::Lock)?
            .map(|state| ContentGuard {
                state,
                protections: &self.protections,
            }))
    }

    /// Guard retirement never waits for a content operation. A new mutation
    /// observes the final release before changing the protected bytes.
    pub(super) fn release_protection(&self) {
        self.protections.release();
    }
}

impl ContentGuard<'_> {
    /// The content mutex excludes existing writers and new protection
    /// acquisition until this count is visible. Releases may run concurrently.
    pub(super) fn acquire_protection(&self) -> Result<(), Error> {
        self.protections
            .acquire()
            .then_some(())
            .ok_or(Error::IdentifierExhausted)
    }

    pub(super) fn known_length(&self) -> Option<u64> {
        self.state.known_length
    }

    pub(super) fn revision(&self) -> ContentRevision {
        ContentRevision::new(self.state.revision)
    }

    /// Backend health must be checked even when the length is already known.
    pub(super) fn length(
        &mut self,
        read_length: impl FnOnce() -> Result<u64, Error>,
    ) -> Result<u64, Error> {
        if let Some(length) = self.state.known_length {
            return Ok(length);
        }
        let length = read_length()?;
        self.state.known_length = Some(length);
        Ok(length)
    }

    /// Failed backend operations may have changed bytes or length. Retire the
    /// old revision before calling them, and reject exhaustion before mutation.
    pub(super) fn begin_mutation(&mut self) -> Result<(), Error> {
        if !self.protections.writable() {
            return Err(Error::Busy);
        }
        self.state.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or(Error::IdentifierExhausted)?;
        self.state.known_length = None;
        Ok(())
    }

    pub(super) fn set_length(&mut self, length: u64) {
        self.state.known_length = Some(length);
    }
}
