// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::file_data_cache::*;
use core::num::NonZeroU64;

fn identity() -> FileIdentity {
    FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MIN),
        NodeIdentity::new(NonZeroU64::MIN),
        ContentRevision::new(NonZeroU64::MIN),
    )
}
fn key() -> CacheKey {
    CacheKey::new(identity(), FilePageIndex::new(0))
}

#[test]
fn dirty_reads_share_pages_and_survive_all_reclaim_paths() -> Result<(), CacheError> {
    let cache = FileDataCache::<FilePage>::try_new(8)?;
    let dirty = crate::require_some(cache.write_page(key(), 4096, &(), |_, page| {
        page.bytes_mut().fill(17);
        Ok::<_, CacheError>(())
    })?);
    let mut output = [0; 16];
    assert_eq!(
        read(&cache, identity(), &(), 4096, 10, &mut output, |_, _| Err(
            "unexpected backend"
        )),
        Ok(16)
    );
    assert_eq!(output, [17; 16]);
    assert_eq!(cache.reclaim_batch(8), 0);
    assert_eq!(cache.maintain_capacity(16)?, Maintenance::Busy);
    assert!(cache.lookup(key())?.is_some());
    drop(dirty);
    assert_eq!(cache.reclaim_batch(8), 1);
    assert!(cache.lookup(key())?.is_none());
    Ok(())
}

#[test]
fn rewrite_keeps_old_reader_and_old_completion_cannot_clean_new_version() -> Result<(), CacheError>
{
    let cache = FileDataCache::<FilePage>::try_new(8)?;
    let old = crate::require_some(cache.write_page(key(), 100, &(), |_, page| {
        page.bytes_mut().fill(11);
        Ok::<_, CacheError>(())
    })?);
    let reader = crate::require_some(cache.lookup(key())?);
    let new = crate::require_some(cache.write_page(key(), 105, &(), |previous, page| {
        let previous = crate::require_some(previous);
        page.bytes_mut().fill(0);
        page.bytes_mut()[..100].copy_from_slice(previous.bytes());
        page.bytes_mut()[95..].fill(22);
        Ok::<_, CacheError>(())
    })?);
    drop(old); // Completion of a previously submitted version.
    assert_eq!(cache.reclaim_batch(8), 0);
    assert_eq!(reader.value().bytes(), &[11; 100]);
    assert_eq!(&new.value().bytes()[..95], &[11; 95]);
    assert_eq!(
        &crate::require_some(cache.lookup(key())?).value().bytes()[95..],
        &[22; 10]
    );
    drop(new);
    assert_eq!(cache.reclaim_batch(8), 1);
    assert_eq!(reader.value().bytes(), &[11; 100]);
    Ok(())
}

#[test]
fn failed_fill_and_capacity_rejection_preserve_accepted_dirty_data() -> Result<(), CacheError> {
    let cache = FileDataCache::<FilePage>::try_new(2)?;
    let old = crate::require_some(cache.write_page(key(), 5, &(), |_, page| {
        page.bytes_mut().copy_from_slice(b"hello");
        Ok::<_, CacheError>(())
    })?);
    assert!(matches!(
        cache.write_page(key(), 5, &(), |_, page| {
            page.bytes_mut().fill(0);
            Err::<(), _>(CacheError::Allocation)
        }),
        Err(CacheError::Allocation)
    ));
    cache.set_admission(false);
    assert!(
        cache
            .write_page(key(), 5, &(), |_, _| Ok::<_, CacheError>(()))?
            .is_none()
    );
    assert_eq!(
        crate::require_some(cache.lookup(key())?).value().bytes(),
        b"hello"
    );
    drop(old);
    cache.set_admission(true);
    assert_eq!(cache.maintain_capacity(4)?, Maintenance::Resized);
    Ok(())
}
