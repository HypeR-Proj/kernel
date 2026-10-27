// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exact-range reads with opportunistic admission of complete file pages.

use hyper::mm::PAGE_SIZE;

use super::{
    CacheAccess, CacheError, CacheKey, FileDataCache, FileIdentity, FilePageIndex, Refill,
};

const PAGE_BYTES: usize = PAGE_SIZE as usize;
const MAX_READ_BYTES: usize = 512 * 1024;

/// One independently reclaimable page of immutable file bytes or filesystem
/// metadata. A typed cache key keeps the payload formats separate. Retaining
/// one page cannot pin a bulk read or an entire directory.
pub(crate) struct FilePage<Owner = ()> {
    bytes: super::storage::PageBytes,
    owner: Owner,
    instructions_published: super::CacheLock<bool>,
}

impl<Owner> FilePage<Owner> {
    #[cfg(not(test))]
    pub(crate) fn owner(&self) -> &Owner {
        &self.owner
    }

    pub(crate) fn try_copy(bytes: &[u8], owner: Owner) -> Result<Self, CacheError> {
        Ok(Self {
            bytes: super::storage::PageBytes::try_copy(bytes)?,
            owner,
            instructions_published: super::CacheLock::new(false),
        })
    }

    pub(crate) fn try_copy_parts(parts: &[&[u8]], owner: Owner) -> Result<Self, CacheError> {
        Ok(Self {
            bytes: super::storage::PageBytes::try_copy_parts(parts)?,
            owner,
            instructions_published: super::CacheLock::new(false),
        })
    }

    pub(crate) fn refill_parts(&mut self, parts: &[&[u8]], owner: Owner) -> Refill {
        self.instructions_published
            .with(|published| *published = false);
        self.bytes.replace_parts(parts);
        self.owner = owner;
        Refill::Ready
    }

    pub(crate) fn refill(&mut self, bytes: &[u8], owner: Owner) -> Refill {
        self.instructions_published
            .with(|published| *published = false);
        self.bytes.replace(bytes);
        // Refill runs only after unique ownership, outside the cache lock.
        // Replacing this generic sidecar can release the old file identity.
        self.owner = owner;
        Refill::Ready
    }

    /// Serializes the first executable exposure across every mapping alias.
    /// The bounded publisher must not sleep or allocate. Refill has exclusive
    /// ownership and resets this state before publishing a different payload.
    pub(crate) fn publish_instructions<E>(
        &self,
        publish: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        self.instructions_published.with(|published| {
            if !*published {
                publish()?;
                *published = true;
            }
            Ok(())
        })
    }

    #[cfg(not(test))]
    pub(crate) fn physical_address(&self) -> hyper::mm::PhysicalAddress {
        self.bytes.physical_address()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReadError<E> {
    Backend(E),
    InvalidBackendResult,
    ArithmeticOverflow,
}

/// Report backend use to sequential prediction without a second cache probe.
pub(crate) fn read_observed<E, Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    file: FileIdentity,
    owner: &Owner,
    file_len: u64,
    offset: u64,
    output: &mut [u8],
    mut read_at: impl FnMut(u64, &mut [u8]) -> Result<usize, E>,
) -> Result<(usize, bool), ReadError<E>> {
    let mut had_miss = false;
    let count = read(cache, file, owner, file_len, offset, output, |at, bytes| {
        had_miss = true;
        read_at(at, bytes)
    })?;
    Ok((count, had_miss))
}

/// The caller retains its file-content lock through this call, including cache
/// lookup and copying. `file`, `file_len`, and every backend read must describe
/// that same content revision. The caller checks backend health before entry;
/// the generic owner retains only the caller's content identity. It must not
/// retain an active open, mount, or filesystem authority.
///
/// Misses read only the requested bytes, in contiguous runs of at most 512 KiB.
/// Short reads terminate the operation. Backend errors always propagate: this
/// buffer is kernel scratch, not a prefix already delivered to userspace.
/// Allocation and publication failures merely skip retention of successful reads.
pub(crate) fn read<E, Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    file: FileIdentity,
    owner: &Owner,
    file_len: u64,
    offset: u64,
    output: &mut [u8],
    mut read_at: impl FnMut(u64, &mut [u8]) -> Result<usize, E>,
) -> Result<usize, ReadError<E>> {
    let requested = u64::try_from(output.len()).map_err(|_| ReadError::ArithmeticOverflow)?;
    offset
        .checked_add(requested)
        .ok_or(ReadError::ArithmeticOverflow)?;
    let length = requested.min(file_len.saturating_sub(offset)) as usize;
    let mut completed = 0;
    while completed < length {
        let current = offset + completed as u64;
        let within = (current % PAGE_SIZE) as usize;
        let page_start = current - within as u64;
        if let Ok(Some(page)) = cache.lookup(key(file, page_start)) {
            let expected = (file_len - page_start).min(PAGE_SIZE) as usize;
            if page.value().bytes.as_slice().len() != expected {
                return Err(ReadError::InvalidBackendResult);
            }
            let count = (expected - within).min(length - completed);
            output[completed..completed + count]
                .copy_from_slice(&page.value().bytes.as_slice()[within..within + count]);
            completed += count;
            continue;
        }

        let count = miss_length(cache, file, current, length - completed);
        let actual = match read_at(current, &mut output[completed..completed + count]) {
            Ok(actual) => actual,
            Err(error) => return Err(ReadError::Backend(error)),
        };
        if actual > count {
            return Err(ReadError::InvalidBackendResult);
        }
        admit(
            cache,
            file,
            owner,
            file_len,
            current,
            &output[completed..completed + actual],
        );
        completed += actual;
        if actual < count {
            break;
        }
    }
    Ok(completed)
}

fn key(file: FileIdentity, page_start: u64) -> CacheKey {
    CacheKey::new(file, FilePageIndex::new(page_start / PAGE_SIZE))
}

/// Stop before the next cached page without splitting a cold run into pages.
fn miss_length<Owner>(
    cache: &FileDataCache<FilePage<Owner>>,
    file: FileIdentity,
    offset: u64,
    remaining: usize,
) -> usize {
    let limit = remaining.min(MAX_READ_BYTES);
    let mut length = (PAGE_SIZE - offset % PAGE_SIZE) as usize;
    while length < limit {
        if matches!(cache.lookup(key(file, offset + length as u64)), Ok(Some(_))) {
            return length;
        }
        length += PAGE_BYTES;
    }
    limit
}

/// Only bytes actually returned at their proper page offset may be retained.
/// Incomplete leading/trailing pages are neither padded nor treated as EOF.
fn admit<Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    file: FileIdentity,
    owner: &Owner,
    file_len: u64,
    offset: u64,
    bytes: &[u8],
) {
    let within = (offset % PAGE_SIZE) as usize;
    let mut consumed = if within == 0 { 0 } else { PAGE_BYTES - within };
    while consumed < bytes.len() {
        let page_start = offset + consumed as u64;
        let count = (file_len - page_start).min(PAGE_SIZE) as usize;
        if bytes.len() - consumed < count {
            break;
        }
        match cache.access(key(file, page_start)) {
            Ok(CacheAccess::Load(reservation)) => {
                let source = &bytes[consumed..consumed + count];
                // Both fresh and recycled payloads retain admission until the
                // data is destroyed. Cache failures never change this read.
                let _ = reservation.fill_and_publish(
                    || FilePage::try_copy(source, owner.clone()),
                    |page| page.refill(source, owner.clone()),
                );
            }
            Ok(CacheAccess::Hit(page)) => drop(page),
            Ok(CacheAccess::LoadInProgress | CacheAccess::CapacityBusy) | Err(_) => {}
        }
        consumed += count;
    }
}
