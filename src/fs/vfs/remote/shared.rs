// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Backend edge of mutable canonical pages. Freeze callbacks always run before
//! acquiring the ordered backend gate; the transport never sees user aliases.

use super::super::shared::{SharedFile, SharedFileIdentity, SharedFilePage};
use super::*;

impl RemoteFs {
    pub(in crate::fs::vfs) fn promote_shared(
        filesystem: &FallibleArc<Self>,
        node: &Node,
        identity: SharedFileIdentity,
        size: u64,
    ) -> Result<FallibleArc<SharedFile>, Error> {
        filesystem.read_status()?;
        let shared = filesystem.shared.as_ref().ok_or(Error::Unsupported)?;
        // Drain old immutable versions before the first mutable frame loads.
        // This is ordered data submission, not a new file or volume sync.
        {
            let mut session = filesystem.session()?;
            let _paths = session.scratch_charge(&filesystem.domain, MAX_PATH_BYTES)?;
            let path = session.path(node)?;
            if session.stat(&path)?.read_only() {
                return Err(Error::Remote(BackendError::ReadOnly));
            }
            session.flush_node(node.id())?;
        }
        shared.promote(filesystem, &node.record, identity, size)
    }
    pub(in crate::fs::vfs) fn shared_page(
        &self,
        file: &FallibleArc<SharedFile>,
        offset: u64,
    ) -> Result<SharedFilePage, Error> {
        self.read_status()?;
        let shared = self.shared.as_ref().ok_or(Error::Unsupported)?;
        shared.page(file, offset)
    }
    pub(in crate::fs::vfs) fn shared_read(
        &self,
        node: u64,
        offset: u64,
        bytes: &mut [u8],
    ) -> Result<usize, Error> {
        let mut session = self.session()?;
        session.bind_path(node)?;
        session.rpc_bound(|volume, path| volume.read_at(path, offset, bytes))
    }
    pub(in crate::fs::vfs) fn shared_write(
        &self,
        node: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let mut session = self.session()?;
        session.check_writeback()?;
        session.bind_path(node)?;
        session.invalidate_entry(node)?;
        // Everything above is local pre-submit preparation. Once ordered RPC
        // begins, failure can follow partial effects and must poison the mount.
        let actual = match session.rpc_bound(|volume, path| {
            volume.write_at(
                path,
                offset,
                &super::super::write_source::WriteSource::Bytes(bytes),
            )
        }) {
            Ok(actual) => actual,
            Err(error) => return Err(session.fail_writeback(error)),
        };
        if actual != bytes.len() {
            return Err(session.fail_writeback(Error::InvalidBackendResult));
        }
        session.writeback.changed = true;
        Ok(())
    }
    pub(in crate::fs::vfs) fn flush_shared(
        &self,
        selected: Option<u64>,
        evict: bool,
    ) -> Result<bool, Error> {
        let Some(shared) = &self.shared else {
            return Ok(false);
        };
        let result = shared.flush(self, selected, evict);
        if self.read_status().is_err() {
            shared.fail()?;
        }
        result
    }
    /// Resolve only the existing canonical binding, without creating a Node
    /// lease which would make the final namespace lease check reject itself.
    /// The caller revalidates this identity under its final backend transaction.
    pub(super) fn shared_binding(
        &self,
        directory: &Node,
        name: EntryName<'_>,
    ) -> Result<Option<u64>, Error> {
        let mut state = self.session()?;
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 3)?;
        let parent = state.path(directory)?;
        let candidate = child(&parent, name.name)?;
        let entry = match state.stat(&candidate) {
            Ok(entry) => entry,
            Err(Error::Missing) => return Ok(None),
            Err(error) => return Err(error),
        };
        let path = join(&parent, entry.name())?;
        Ok(state.records.find_path(&path).map(|record| record.id))
    }
    pub(in crate::fs::vfs) fn shared_has_resident(&self, node: u64) -> Result<bool, Error> {
        self.shared
            .as_ref()
            .map_or(Ok(false), |shared| shared.has_resident(node))
    }
    /// Pressure is opportunistic; retained copy plans may pin every frame.
    /// Required extent/namespace retirement uses `flush_shared` directly and
    /// receives Busy until those pins are released.
    pub(in crate::fs::vfs) fn reclaim_shared(&self) -> Result<bool, Error> {
        let Some(shared) = &self.shared else {
            return Ok(false);
        };
        let result = shared.reclaim(self);
        if self.read_status().is_err() {
            shared.fail()?;
        }
        result
    }
    pub(in crate::fs::vfs) fn prepare_shared_write(&self, length: usize) -> Result<(), Error> {
        let pages = length / hyper::mm::PAGE_SIZE as usize + 2;
        if self
            .shared
            .as_ref()
            .is_some_and(|shared| shared.needs_capacity(pages))
        {
            self.reclaim_shared()?;
        }
        Ok(())
    }
    pub(super) fn revoke_failed_shared(&self) -> Result<u64, Error> {
        self.shared.as_ref().map_or(Ok(0), |shared| shared.fail())
    }
}
