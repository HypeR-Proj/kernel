// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional directory and name retention in the same
//! reclaimable pages as file data. Callers hold the volume mutex and check
//! backend health before hits.
//! Identities contain the mount generation and its current metadata revision;
//! mutations retire that revision before a possibly partial backend change.

#[cfg(test)]
use crate::file_data_cache as io_cache;
#[cfg(not(test))]
use crate::fs::cache as io_cache;

use hyper_filesystem::protocol::{
    DirectoryBatch, DirectoryCursor, Entry, RESPONSE_BYTES, entry_decode_into, entry_encode_into,
};
use io_cache::{CacheAccess, CacheKey, CachedPage, FileDataCache, FileIdentity, FilePage};

const KEY_LENGTH_BYTES: usize = core::mem::size_of::<u64>();

pub(super) fn lookup_entry<Owner>(
    cache: &FileDataCache<FilePage<Owner>>,
    identity: FileIdentity,
    key: &str,
    output: &mut Entry,
) -> bool {
    let Ok(Some(page)) = cache.lookup(CacheKey::lookup(identity, hash(key.bytes()))) else {
        return false;
    };
    let bytes = page.value().bytes();
    let Some(prefix) = bytes.get(..KEY_LENGTH_BYTES) else {
        return false;
    };
    let Ok(prefix) = prefix.try_into() else {
        return false;
    };
    let Ok(length) = usize::try_from(u64::from_le_bytes(prefix)) else {
        return false;
    };
    let Some(end) = KEY_LENGTH_BYTES.checked_add(length) else {
        return false;
    };
    if bytes.get(KEY_LENGTH_BYTES..end) != Some(key.as_bytes()) {
        // The key hash is only placement. Collisions cannot substitute a
        // different name, alias, or path for the requested object.
        return false;
    }
    bytes
        .get(end..)
        .is_some_and(|entry| entry_decode_into(entry, output).is_some())
}

/// Retain a canonical entry under an exact request name or path. Oversized
/// keys, pressure, competing fills, and hash collisions simply skip admission.
#[inline(never)]
pub(super) fn retain_entry<Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    identity: FileIdentity,
    key: &str,
    entry: &Entry,
    owner: &Owner,
) {
    // The compact Entry fits inside its existing fixed response bound. Keep
    // this scratch separate from the path; no page-sized temporary is needed.
    let mut encoded = [0_u8; RESPONSE_BYTES];
    let Some(length) = entry_encode_into(entry, &mut encoded) else {
        return;
    };
    let Some(total) = KEY_LENGTH_BYTES
        .checked_add(key.len())
        .and_then(|value| value.checked_add(length))
    else {
        return;
    };
    if total > hyper::mm::PAGE_SIZE as usize {
        return;
    }
    let Ok(CacheAccess::Load(reservation)) =
        cache.access(CacheKey::lookup(identity, hash(key.bytes())))
    else {
        return;
    };
    let prefix = (key.len() as u64).to_le_bytes();
    let parts: [&[u8]; 3] = [&prefix, key.as_bytes(), &encoded[..length]];
    let _ = reservation.fill_and_publish(
        || FilePage::try_copy_parts(&parts, owner.clone()),
        |page| page.refill_parts(&parts, owner.clone()),
    );
}

pub(super) fn lookup_batch<Owner>(
    cache: &FileDataCache<FilePage<Owner>>,
    identity: FileIdentity,
    start: DirectoryCursor,
) -> Option<CachedPage<FilePage<Owner>>> {
    let page = cache
        .lookup(CacheKey::directory(identity, cursor_hash(start)))
        .ok()??;
    let batch = DirectoryBatch::decode(page.value().bytes())?;
    // A cursor has four words; reducing it for index placement must never
    // turn a collision into acceptance of another physical continuation.
    (batch.start_cursor() == start).then_some(page)
}

pub(super) fn retain_batch<Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    identity: FileIdentity,
    start: DirectoryCursor,
    bytes: &[u8],
    owner: &Owner,
) {
    if !DirectoryBatch::decode(bytes).is_some_and(|batch| batch.start_cursor() == start) {
        return;
    }
    if let Ok(CacheAccess::Load(reservation)) =
        cache.access(CacheKey::directory(identity, cursor_hash(start)))
    {
        let _ = reservation.fill_and_publish(
            || FilePage::try_copy(bytes, owner.clone()),
            |page| page.refill(bytes, owner.clone()),
        );
    }
}

fn cursor_hash(cursor: DirectoryCursor) -> u64 {
    hash(cursor.words.into_iter().flat_map(u64::to_le_bytes))
}

fn hash(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
