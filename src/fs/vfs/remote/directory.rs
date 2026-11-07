// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded streaming enumeration with discardable shared directory pages.

use hyper::mm::FallibleArc;
use hyper_filesystem::protocol::{DirectoryBatch, Entry};
use {
    crate::fs::primitives::MAX_NAME_BYTES, crate::fs::primitives::MAX_PATH_BYTES,
    crate::fs::primitives::Name,
};

use crate::fs::vfs::directory::RemoteDirectoryPosition;
use {crate::fs::cache::FileDataCache, crate::fs::cache::FileIdentity};
use {crate::fs::vfs::file_record::CachePage, crate::fs::vfs::file_record::FileRecord};
use {
    crate::fs::vfs::objects::DIRECTORY_PAGE_CAPACITY,
    crate::fs::vfs::objects::DirectoryEntrySnapshot, crate::fs::vfs::objects::DirectoryPage,
};

use super::{Error, Node, RemoteFs, State, attributes, cache, join, map};

impl RemoteFs {
    #[inline(never)]
    pub(in crate::fs::vfs) fn read_directory(
        &self,
        directory: &FallibleArc<Node>,
        position: &RemoteDirectoryPosition,
        page: &mut DirectoryPage,
    ) -> Result<RemoteDirectoryPosition, Error> {
        let mut state = self.session()?;
        self.read_status()?;
        let revision = directory
            .directory_revision
            .load(core::sync::atomic::Ordering::Relaxed);
        if position
            .revision
            .is_some_and(|observed| observed != revision)
        {
            return Err(Error::InvalidDirectoryCookie);
        }
        let _paths = state.scratch_charge(&self.domain, MAX_PATH_BYTES * 2)?;
        let path = state.path(directory)?;
        if position.revision.is_none() && !state.is_directory(&path)? {
            return Err(Error::NotDirectory);
        }
        let mut next = *position;
        next.revision = Some(revision);
        let identity = state.metadata_identity(directory.id())?;
        let lookup_identity = state.entry_fallback_identity()?;
        let metadata_revision = state
            .records
            .get(directory.id())
            .ok_or(Error::Missing)?
            .directory_revision;
        if next.batch_revision != metadata_revision {
            next.batch_start = next.checkpoint;
            next.batch_index = 0;
            next.batch_revision = metadata_revision;
        }
        page.len = 0;
        page.next_cookie = 0;

        // One volume transaction covers lookup, backend access, attribute
        // publication and cursor advancement. Mutations cannot publish a stale
        // batch or recycle a FAT cluster while it is used as a checkpoint.
        while page.len < DIRECTORY_PAGE_CAPACITY {
            let cached = (next.batch_revision == metadata_revision)
                .then(|| cache::lookup_batch(&state.cache, identity, next.batch_start))
                .flatten();
            let count = if cached.is_none() {
                next.batch_start = next.checkpoint;
                next.batch_index = 0;
                state.finish_batches()?;
                let count = state.backend_rpc(|backend| {
                    backend
                        .volume
                        .read_directory(&path, next.checkpoint, &mut backend.directory)
                        .map_err(map)
                })?;
                cache::retain_batch(
                    &state.cache,
                    identity,
                    next.batch_start,
                    &state.backend.directory[..count],
                    &directory.record,
                );
                count
            } else {
                0
            };
            let super::Session {
                state: Some(guard),
                backend,
                ..
            } = &mut state
            else {
                return Err(Error::InvalidBackendResult);
            };
            let State {
                cache: pages,
                root_record,
                metadata,
                records,
                writeback,
                ..
            } = &mut **guard;
            let bytes = if let Some(cached) = &cached {
                cached.value().bytes()
            } else {
                &backend.directory[..count]
            };
            next.batch_revision = metadata_revision;
            let Some(batch) = DirectoryBatch::decode(bytes) else {
                backend.volume.health.fail();
                return Err(Error::InvalidBackendResult);
            };
            if batch.start_cursor() != next.batch_start || next.batch_index > batch.len() {
                backend.volume.health.fail();
                return Err(Error::InvalidBackendResult);
            }
            let context = AttributeAdmission {
                cache: pages,
                identity: lookup_identity,
                owner: root_record,
                directory: &path,
                records,
                writeback,
            };
            let result = consume_batch(&batch, &mut next, page, metadata, &context);
            if matches!(result, Err(Error::InvalidBackendResult)) {
                backend.volume.health.fail();
            }
            if result? {
                return Ok(next);
            }
        }
        page.next_cookie = 1;
        Ok(next)
    }
}

struct AttributeAdmission<'a> {
    cache: &'a FileDataCache<CachePage>,
    identity: FileIdentity,
    owner: &'a FallibleArc<FileRecord>,
    directory: &'a str,
    records: &'a super::Records,
    writeback: &'a super::writeback::Writeback,
}

/// Returns true only at EOF. No Entry/Name/Node owner survives this call; the
/// cached attribute page retains the filesystem's plain accounting sidecar.
fn consume_batch(
    batch: &DirectoryBatch<'_>,
    position: &mut RemoteDirectoryPosition,
    output: &mut DirectoryPage,
    scratch: &mut Entry,
    admission: &AttributeAdmission<'_>,
) -> Result<bool, Error> {
    for record in batch.records().skip(position.batch_index) {
        let name = record.name();
        // The backend contract accommodates FAT's UTF-16 names, whose UTF-8
        // spelling can exceed the Native ABI's component capacity. That is an
        // unsupported enumeration result, not a corrupt or failed mount.
        if name.len() > MAX_NAME_BYTES {
            return Err(Error::Unsupported);
        }
        if Name::new(name).is_err() {
            return Err(Error::InvalidBackendResult);
        }
        record.entry_into(scratch);
        // Retain only backend attributes as the cache base. Dirty overlays are
        // applied to this listing result afterwards, never committed into a base.
        if let Ok(path) = join(admission.directory, name) {
            cache::retain_entry(
                admission.cache,
                admission.identity,
                &path,
                scratch,
                admission.owner,
            );
        }
        // The backend has not yet seen cached extensions. Canonical bindings
        // identify the accepted size without flushing data for an ls/stat.
        if let Some(binding) = admission.records.iter().find(|binding| {
            let (parent, base) = binding.path.rsplit_once('/').unwrap_or(("", &binding.path));
            parent == admission.directory && base == name
        }) {
            admission.writeback.overlay(binding.id, scratch);
        }
        let mut name_bytes = [0; MAX_NAME_BYTES];
        name_bytes[..name.len()].copy_from_slice(name.as_bytes());
        output.entries[output.len] = Some(DirectoryEntrySnapshot {
            name: name_bytes,
            name_length: name.len() as u32,
            attributes: attributes(scratch),
        });
        output.len += 1;
        position.checkpoint = record.next_cursor();
        position.batch_index += 1;

        if output.len == DIRECTORY_PAGE_CAPACITY {
            break;
        }
    }
    if position.batch_index == batch.len() {
        if batch.eof() {
            output.next_cookie = 0;
            return Ok(true);
        }
        position.batch_start = position.checkpoint;
        position.batch_index = 0;
    }
    Ok(false)
}
