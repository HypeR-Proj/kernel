// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Ordered backend ownership, separate from short cache/namespace state access.
//! Lock order is file content -> backend -> state. Cache-only publication takes
//! state alone. Every RPC releases state, and completion never acquires content.

use super::*;
use crate::sync::MutexGuard;
use core::ops::{Deref, DerefMut};

pub(super) struct Backend {
    pub(super) volume: Transport,
    pub(super) metadata: Box<Entry>,
    pub(super) path: String,
    pub(super) directory: Box<[u8; hyper_filesystem::protocol::DIRECTORY_BATCH_BYTES]>,
    pub(super) batches: writeback::Batches,
    pub(super) partial: Option<Box<[u8; hyper::mm::PAGE_SIZE as usize]>>,
}
impl Backend {
    pub(super) fn new(
        volume: Transport,
        domain: &ResourceDomain,
        enabled: bool,
    ) -> Result<Box<Self>, Error> {
        let mut path = String::new();
        path.try_reserve_exact(MAX_PATH_BYTES)
            .map_err(|_| Error::Allocation)?;
        hyper::mm::try_box(Self {
            volume,
            path,
            metadata: metadata_scratch()?,
            directory: byte_buffer()?,
            batches: writeback::Batches::new(domain, enabled)?,
            partial: if enabled { Some(byte_buffer()?) } else { None },
        })
        .map_err(|_| Error::Allocation)
    }
}

/// Mount-owned scratch is initialized in its charged heap allocation. Passing
/// `[0; N]` into `try_box` would reserve N bytes in every caller's stack frame,
/// even when the optional partial-page buffer is disabled for this provider.
fn byte_buffer<const N: usize>() -> Result<Box<[u8; N]>, Error> {
    let mut buffer = hyper::mm::try_box_uninit::<[u8; N]>()?;
    // SAFETY: the Box uniquely owns N writable bytes, and every byte is
    // initialized before conversion; all bit patterns are valid for u8.
    unsafe {
        buffer.as_mut_ptr().cast::<u8>().write_bytes(0, N);
        Ok(buffer.assume_init())
    }
}

pub(super) struct Session<'a> {
    pub(super) filesystem: &'a RemoteFs,
    pub(super) backend: MutexGuard<'a, Box<Backend>>,
    pub(super) state: Option<MutexGuard<'a, State>>,
    fenced: bool,
}
impl RemoteFs {
    pub(super) fn session(&self) -> Result<Session<'_>, Error> {
        let backend = self.backend.lock().map_err(Error::Lock)?;
        let state = self.state.lock().map_err(Error::Lock)?;
        Ok(Session {
            filesystem: self,
            backend,
            state: Some(state),
            fenced: false,
        })
    }
    pub(super) fn try_session(&self) -> Result<Option<Session<'_>>, Error> {
        let Some(backend) = self.backend.try_lock().map_err(Error::Lock)? else {
            return Ok(None);
        };
        let Some(state) = self.state.try_lock().map_err(Error::Lock)? else {
            return Ok(None);
        };
        Ok(Some(Session {
            filesystem: self,
            backend,
            state: Some(state),
            fenced: false,
        }))
    }
}
impl Deref for Session<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        match &self.state {
            Some(state) => state,
            None => hyper::debug::invariant_failure("filesystem session state is not held"),
        }
    }
}
impl DerefMut for Session<'_> {
    fn deref_mut(&mut self) -> &mut State {
        match &mut self.state {
            Some(state) => state,
            None => hyper::debug::invariant_failure("filesystem session state is not held"),
        }
    }
}
impl Session<'_> {
    pub(super) fn backend_rpc<T>(
        &mut self,
        call: impl FnOnce(&mut Backend) -> Result<T, Error>,
    ) -> Result<T, Error> {
        drop(self.state.take());
        let result = call(&mut self.backend);
        // Once an RPC may have committed, restoring its owner is cleanup, not
        // cancellable admission. A stop request interrupts a particular wait;
        // re-arm until State is recovered, then observe stop at syscall return.
        self.state = Some(loop {
            match self.filesystem.state.lock() {
                Ok(state) => break state,
                Err(crate::sync::Error::WaitInterrupted(crate::task::WaitOutcome::Cancelled)) => {
                    continue;
                }
                Err(_) => {
                    hyper::debug::invariant_failure("filesystem RPC state restoration failed")
                }
            }
        });
        result
    }
    pub(super) fn rpc<T>(
        &mut self,
        call: impl FnOnce(&mut Transport) -> Result<T, BackendError>,
    ) -> Result<T, Error> {
        self.finish_batches()?;
        self.backend_rpc(|backend| call(&mut backend.volume).map_err(map))
    }
    pub(super) fn bind_path(&mut self, node: u64) -> Result<(), Error> {
        let Some(state) = &self.state else {
            return Err(Error::InvalidBackendResult);
        };
        let path = state.records.path(node)?;
        if path.len() > MAX_PATH_BYTES {
            return Err(Error::InvalidBackendResult);
        }
        self.backend.path.clear();
        self.backend.path.push_str(path);
        Ok(())
    }
    pub(super) fn rpc_bound<T>(
        &mut self,
        call: impl FnOnce(&mut Transport, &str) -> Result<T, BackendError>,
    ) -> Result<T, Error> {
        self.finish_batches()?;
        self.backend_rpc(|backend| call(&mut backend.volume, &backend.path).map_err(map))
    }
    pub(super) fn fence_cache(&mut self) {
        self.writeback.fenced = true;
        self.fenced = true;
    }
    pub(super) fn stat(&mut self, path: &str) -> Result<&Entry, Error> {
        self.check_writeback()?;
        if !self.cached_stat(path)? {
            self.rpc(|volume| volume.health.check())?;
            self.backend_rpc(|backend| {
                backend
                    .volume
                    .stat_into(path, &mut backend.metadata)
                    .map_err(map)
            })?;
            let identity = self.entry_identity(path)?;
            let backend = &self.backend;
            let Some(state) = &mut self.state else {
                return Err(Error::InvalidBackendResult);
            };
            state
                .metadata
                .as_mut()
                .clone_from(backend.metadata.as_ref());
            cache::retain_entry(
                &state.cache,
                identity,
                path,
                &state.metadata,
                &state.root_record,
            );
            state.overlay_metadata(path);
        }
        Ok(&self.metadata)
    }
    pub(super) fn is_directory(&mut self, path: &str) -> Result<bool, Error> {
        Ok(self.stat(path)?.directory())
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        if self.fenced {
            if let Some(state) = &mut self.state {
                state.writeback.fenced = false;
            } else {
                hyper::debug::invariant_failure("filesystem fence lost restored state");
            }
        }
    }
}

impl State {
    pub(super) fn entry_identity(&self, path: &str) -> Result<FileIdentity, Error> {
        if let Some(record) = self.records.find_path(path) {
            let node = record
                .id
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or(Error::IdentifierExhausted)?;
            let revision =
                NonZeroU64::new(record.metadata_revision).ok_or(Error::IdentifierExhausted)?;
            Ok(FileIdentity::new(
                self.generation,
                NodeIdentity::new(node),
                ContentRevision::new(revision),
            ))
        } else {
            self.entry_fallback_identity()
        }
    }
    pub(super) fn overlay_metadata(&mut self, path: &str) {
        if let Some(record) = self.records.find_path(path) {
            self.writeback.overlay(record.id, &mut self.metadata);
        }
    }
    pub(super) fn cached_stat_node(&mut self, node: u64) -> Result<bool, Error> {
        let record = self.records.get(node).ok_or(Error::Missing)?;
        let identity = self.metadata_identity_at(node, record.metadata_revision)?;
        let fallback = self.entry_fallback_identity()?;
        if !cache::lookup_entry_with_fallback(
            &self.cache,
            identity,
            fallback,
            &record.path,
            &mut self.metadata,
            &self.root_record,
        ) {
            return Ok(false);
        }
        self.writeback.overlay(node, &mut self.metadata);
        Ok(true)
    }
    pub(super) fn cached_stat(&mut self, path: &str) -> Result<bool, Error> {
        let identity = self.entry_identity(path)?;
        let fallback = self.entry_fallback_identity()?;
        if !cache::lookup_entry_with_fallback(
            &self.cache,
            identity,
            fallback,
            path,
            &mut self.metadata,
            &self.root_record,
        ) {
            return Ok(false);
        }
        self.overlay_metadata(path);
        Ok(true)
    }
}
impl State {
    pub(super) fn complete_attributes(
        &mut self,
        node: u64,
        path: &str,
        end: u64,
        modified: Option<hyper_filesystem::Timestamp>,
    ) -> Result<(), Error> {
        let identity = self.entry_identity(path)?;
        // Retain only authoritative completed attributes as the new cached base;
        // pending/newer versions remain in the separately accounted overlay.
        let state = self;
        let cached = cache::lookup_entry(&state.cache, identity, path, &mut state.metadata);
        state.metadata_revision = state
            .metadata_revision
            .checked_add(1)
            .ok_or(Error::IdentifierExhausted)?;
        let revision = state.metadata_revision;
        let record = state.records.get_mut(node).ok_or(Error::Missing)?;
        record.metadata_revision = revision;
        state.invalidate_parent_listing(node, revision)?;
        if cached {
            state.metadata.size = state.metadata.size.max(end);
            if let Some(modified) = modified {
                state.metadata.modified = Some(modified);
            }
            let identity = state.entry_identity(path)?;
            cache::retain_entry(
                &state.cache,
                identity,
                path,
                &state.metadata,
                &state.root_record,
            );
        }
        Ok(())
    }
}
