// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Demand reads into exclusive cache pages without a payload staging buffer.

use alloc::vec::Vec;
use core::alloc::Layout;

use hyper::mm::PAGE_SIZE;

use super::{
    CacheAccess, CacheKey, CachedPage, FileDataCache, FileIdentity, FilePage, FilePageIndex,
    PendingPage, ReadError,
};

const MAX_BYTES: usize = 512 * 1024;

/// Short-lived immutable pins, released after the syscall copies into user
/// memory. They do not establish persistent file mappings or residency locks.
/// Index eviction remains possible; physical recovery waits for these pins.
pub(crate) struct PageRead<Owner> {
    pages: Vec<CachedPage<FilePage<Owner>>>,
    first_offset: usize,
    pub(crate) length: usize,
    pub(crate) had_miss: bool,
}

impl<Owner> PageRead<Owner> {
    pub(crate) fn pages(&self) -> impl Iterator<Item = &[u8]> + Clone {
        self.pages.iter().scan(
            (self.first_offset, self.length),
            |(skip, remaining), page| {
                let bytes = page.value().bytes();
                let start = (*skip).min(bytes.len());
                *skip -= start;
                let length = (*remaining).min(bytes.len() - start);
                *remaining -= length;
                Some(&bytes[start..start + length])
            },
        )
    }
}

enum Slot<'a, Owner> {
    Hit(CachedPage<FilePage<Owner>>),
    Fill(PendingPage<'a, FilePage<Owner>>),
}

/// Fills complete file pages covering the request, including an exact EOF tail,
/// in bounded scatter batches, but exposes only the requested bytes. The caller
/// holds the file content gate through this operation. Partial ELF segment reads
/// must not leave persistent holes in the cache at each segment boundary.
/// `None` selects the ordinary charged scratch path for oversized requests,
/// cache admission pressure, or short reads confined to the rounded prefix.
/// All reservations are released before that fallback.
///
/// The backend sees only exclusive unpublished page slices. It must finish all
/// access before returning. A short read publishes only complete pages; its
/// partial final page remains private to this one result. Failed fills never
/// enter the cache. No provider receives writable aliases of published pages.
pub(crate) fn read_pages<E, Owner: Clone>(
    cache: &FileDataCache<FilePage<Owner>>,
    file: FileIdentity,
    owner: &Owner,
    file_len: u64,
    offset: u64,
    requested: usize,
    mut read_at: impl FnMut(u64, &mut [&mut [u8]]) -> Result<usize, E>,
) -> Result<Option<PageRead<Owner>>, ReadError<E>> {
    let length = requested.min(file_len.saturating_sub(offset).min(usize::MAX as u64) as usize);
    let end = offset
        .checked_add(length as u64)
        .ok_or(ReadError::ArithmeticOverflow)?;
    let first_offset = (offset % PAGE_SIZE) as usize;
    let offset = offset - first_offset as u64;
    // Subtraction from the known file length also avoids rounding u64::MAX up.
    let padding = (PAGE_SIZE - end % PAGE_SIZE) % PAGE_SIZE;
    let end = end + padding.min(file_len.saturating_sub(end));
    let covered = if length == 0 { 0 } else { end - offset };
    if covered > MAX_BYTES as u64 {
        return Ok(None);
    }
    let count = (covered as usize).div_ceil(PAGE_SIZE as usize);
    let layouts = [
        Layout::array::<Slot<'_, Owner>>(count),
        Layout::array::<CachedPage<FilePage<Owner>>>(count),
        Layout::array::<&mut [u8]>(count),
    ];
    let [Ok(slots_layout), Ok(pages_layout), Ok(slices_layout)] = layouts else {
        return Ok(None);
    };
    let Ok((reservation, _)) = super::memory::reserve(&[slots_layout, pages_layout, slices_layout])
    else {
        return Ok(None);
    };
    let Ok(mut slots) = super::memory::vector(count) else {
        return Ok(None);
    };
    let Ok(mut pages) = super::memory::vector(count) else {
        return Ok(None);
    };
    let mut had_miss = false;
    for index in 0..count {
        let at = offset + index as u64 * PAGE_SIZE;
        let expected = (file_len - at).min(PAGE_SIZE) as usize;
        let key = CacheKey::new(file, FilePageIndex::new(at / PAGE_SIZE));
        match cache.access(key) {
            Ok(CacheAccess::Hit(page)) => {
                if page.value().bytes().len() != expected {
                    return Err(ReadError::InvalidBackendResult);
                }
                slots.push(Slot::Hit(page));
            }
            Ok(CacheAccess::Load(load)) => {
                let Ok(page) = load.prepare_fill(
                    || FilePage::try_empty(expected, owner.clone()),
                    |page| page.prepare_fill(expected, owner.clone()),
                ) else {
                    return Ok(None);
                };
                slots.push(Slot::Fill(page));
                had_miss = true;
            }
            _ => return Ok(None),
        }
    }
    let mut index = 0;
    let mut completed = 0;
    while index < slots.len() {
        if let Slot::Hit(page) = &slots[index] {
            completed += page.value().bytes().len();
            index += 1;
            continue;
        }
        let end = (index + 1..slots.len())
            .find(|&next| matches!(slots[next], Slot::Hit(_)))
            .unwrap_or(slots.len());
        let (actual, expected) = {
            // This temporary contains references only, never payload bytes.
            let Ok(mut slices) = super::memory::vector(end - index) else {
                return Ok(None);
            };
            let mut expected = 0;
            for slot in &mut slots[index..end] {
                if let Slot::Fill(page) = slot {
                    let bytes = page.value_mut().bytes_mut();
                    expected += bytes.len();
                    slices.push(bytes);
                }
            }
            let actual =
                read_at(offset + completed as u64, &mut slices).map_err(ReadError::Backend)?;
            (actual, expected)
        };
        if actual > expected {
            return Err(ReadError::InvalidBackendResult);
        }
        completed += actual;
        if actual != expected {
            break;
        }
        index = end;
    }
    // A short read may stop entirely within the rounded leading prefix. Zero
    // requested bytes then says nothing about EOF at the caller's offset.
    // Release the unpublished pages and let demand reads use their exact range.
    if length != 0 && first_offset != 0 && completed <= first_offset {
        return Ok(None);
    }
    drop(reservation);
    let mut remaining = completed;
    for slot in slots {
        if remaining == 0 {
            break;
        }
        match slot {
            Slot::Hit(page) => {
                remaining -= page.value().bytes().len();
                pages.push(page);
            }
            Slot::Fill(mut pending) => {
                let expected = pending.value_mut().bytes().len();
                let actual = expected.min(remaining);
                pending.value_mut().truncate(actual);
                pages.push(pending.finish(actual == expected));
                remaining -= actual;
            }
        }
    }
    Ok(Some(PageRead {
        pages,
        first_offset,
        length: completed.saturating_sub(first_offset).min(length),
        had_miss,
    }))
}
