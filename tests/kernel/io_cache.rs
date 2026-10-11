// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduled cache growth, pressure eviction, and final reader ownership.

use core::num::NonZeroU64;

use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::io_cache::{
    CacheAccess, CacheError, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePageIndex,
    FilesystemGeneration, NodeIdentity, Refill, worker,
};

static CONTENTS: [u8; 4096] = [0xa5; 4096];

#[derive(Debug)]
pub(super) enum Error {
    Allocation,
    Cache,
    Progress,
    IdentityLifetime,
    PinnedBytes,
    PhysicalPages,
    Quiescence,
}

struct Pressure;

impl Pressure {
    fn start() -> Self {
        worker::pressure_for_test(true);
        Self
    }
}

impl Drop for Pressure {
    fn drop(&mut self) {
        worker::pressure_for_test(false);
    }
}

pub(super) fn run() -> Result<(), Error> {
    let cache = crate::kernel::vfs::file_cache().ok_or(Error::Cache)?;
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Allocation)?;
    let record =
        crate::kernel::vfs::cache_test_record(1, &domain).map_err(|_| Error::Allocation)?;
    let weak = record.downgrade();
    let initial_capacity = cache.usage().capacity;
    let identity = FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MAX),
        NodeIdentity::new(NonZeroU64::MIN),
        ContentRevision::new(NonZeroU64::MIN),
    );
    let mut last = CacheKey::new(identity, FilePageIndex::new(0));
    for index in 0..=initial_capacity {
        last = CacheKey::new(identity, FilePageIndex::new(index as u64));
        wait(|| match cache.access(last) {
            Ok(CacheAccess::Hit(_)) => true,
            Ok(CacheAccess::Load(reservation)) => reservation
                .fill_and_publish(
                    || crate::kernel::vfs::CachePage::try_copy(&CONTENTS, record.clone()),
                    |_| Refill::Recreate,
                )
                .is_ok(),
            _ => false,
        })?;
    }
    wait(|| cache.usage().capacity > initial_capacity)?;
    let pinned = cache
        .lookup(last)
        .map_err(|_| Error::Cache)?
        .ok_or(Error::Cache)?;
    drop(record);
    if weak.upgrade().is_none() {
        return Err(Error::IdentityLifetime);
    }

    let pressure = Pressure::start();
    wait(|| cache.usage().clean == 0 && cache.usage().loading == 0)?;
    // Removing the index owner does not free a page still being read, or the
    // file identity carried by that immutable payload.
    if pinned.value().bytes() != CONTENTS || weak.upgrade().is_none() {
        return Err(Error::PinnedBytes);
    }
    wait(|| {
        crate::kernel::mm::cache_memory::availability()
            .is_some_and(|memory| memory.file_cache_pages == 1)
    })?;
    drop(pinned);
    wait(|| {
        crate::kernel::mm::cache_memory::availability()
            .is_some_and(|memory| memory.file_cache_pages == 0)
    })?;
    // Page accounting can reach zero before another CPU finishes dropping
    // that payload's identity sidecar. Require eventual final ownership loss.
    wait(|| weak.upgrade().is_none()).map_err(|_| Error::IdentityLifetime)?;
    wait(|| cache.usage().capacity == 64)?;
    drop(pressure);
    wait(|| !worker::draining_for_test())?;
    super::support::quiesce_workers().map_err(|_| Error::Quiescence)?;
    late_publication_under_pressure(cache, &domain)?;
    directory_pages_under_pressure(cache, &domain)?;
    scheduled_domain_reclaim(cache, &domain)?;
    scheduled_impossible_order(cache, &domain)?;
    if crate::kernel::mm::cache_memory::availability()
        .is_none_or(|memory| memory.file_cache_pages != 0)
    {
        return Err(Error::PhysicalPages);
    }
    Ok(())
}

/// A live directory identity and its trusted continuation do not pin metadata
/// pages. The actual reclaim worker must discard both wire batches and lookup
/// payloads, then reject optional retention while the pressure episode lasts.
#[inline(never)]
fn directory_pages_under_pressure(
    cache: &FileDataCache<crate::kernel::vfs::CachePage>,
    domain: &ResourceDomain,
) -> Result<(), Error> {
    use hyper_filesystem::protocol::{
        DIRECTORY_BATCH_BYTES, DirectoryBatch, DirectoryBatchBuilder, DirectoryCursor, Entry,
        RESPONSE_BYTES, entry_encode_into,
    };

    let baseline = domain.usage().committed(ResourceKind::KernelMemoryBytes);
    let stream_identity =
        crate::kernel::vfs::cache_test_record(6, domain).map_err(|_| Error::Allocation)?;
    let weak = stream_identity.downgrade();
    let identity = FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MAX),
        NodeIdentity::new(NonZeroU64::new(6).ok_or(Error::Cache)?),
        ContentRevision::new(NonZeroU64::MIN),
    );
    let mut entry = Entry::empty();
    entry.name[..5].copy_from_slice(b"child");
    entry.name_len = 5;
    let mut bytes = [0; DIRECTORY_BATCH_BYTES];
    let mut builder =
        DirectoryBatchBuilder::new(&mut bytes, DirectoryCursor::START).ok_or(Error::Cache)?;
    let continuation = DirectoryCursor {
        words: [17, 32, 7, 11],
    };
    if builder.push(&entry, continuation) != Some(true) {
        return Err(Error::Cache);
    }
    let length = builder.finish(false).ok_or(Error::Cache)?;
    let directory_key = CacheKey::directory(identity, 0);
    let lookup_key = CacheKey::lookup(identity, 0);
    let mut encoded = [0; RESPONSE_BYTES];
    let encoded_length = entry_encode_into(&entry, &mut encoded).ok_or(Error::Cache)?;
    let name_length = 5_u64.to_le_bytes();
    for (key, parts) in [
        (directory_key, [&bytes[..length], &[][..], &[][..]]),
        (
            lookup_key,
            [
                &name_length[..],
                b"child".as_slice(),
                &encoded[..encoded_length],
            ],
        ),
    ] {
        let CacheAccess::Load(reservation) = cache.access(key).map_err(|_| Error::Cache)? else {
            return Err(Error::Cache);
        };
        drop(
            reservation
                .fill_and_publish(
                    || {
                        crate::kernel::vfs::CachePage::try_copy_parts(
                            &parts,
                            stream_identity.clone(),
                        )
                    },
                    |page| page.refill_parts(&parts, stream_identity.clone()),
                )
                .map_err(|_| Error::Cache)?,
        );
    }
    let checkpoint = {
        let page = cache
            .lookup(directory_key)
            .map_err(|_| Error::Cache)?
            .ok_or(Error::Cache)?;
        let batch = DirectoryBatch::decode(page.value().bytes()).ok_or(Error::Cache)?;
        batch.records().next().ok_or(Error::Cache)?.next_cursor()
    };
    // Only scalar continuation state and the live directory identity survive
    // here, exactly the ownership boundary kept by a DirectoryStream.
    let pressure = Pressure::start();
    wait(|| {
        worker::draining_for_test() && cache.usage().clean == 0 && cache.usage().live_payloads == 0
    })?;
    wait(|| {
        crate::kernel::mm::cache_memory::availability()
            .is_some_and(|memory| memory.file_cache_pages == 0)
    })?;
    if checkpoint != continuation
        || weak.upgrade().is_none()
        || cache
            .lookup(directory_key)
            .map_err(|_| Error::Cache)?
            .is_some()
        || cache
            .lookup(lookup_key)
            .map_err(|_| Error::Cache)?
            .is_some()
        || !matches!(cache.access(directory_key), Ok(CacheAccess::CapacityBusy))
    {
        return Err(Error::IdentityLifetime);
    }
    drop(stream_identity);
    if weak.upgrade().is_some()
        || domain.usage().committed(ResourceKind::KernelMemoryBytes) != baseline
    {
        return Err(Error::IdentityLifetime);
    }
    drop(pressure);
    wait(|| !worker::draining_for_test())?;
    super::support::quiesce_workers().map_err(|_| Error::Quiescence)?;
    Ok(())
}

fn fill_request_pages(
    cache: &FileDataCache<crate::kernel::vfs::CachePage>,
    domain: &ResourceDomain,
    node: u64,
    pages: u64,
) -> Result<CacheKey, Error> {
    let record =
        crate::kernel::vfs::cache_test_record(node, domain).map_err(|_| Error::Allocation)?;
    let identity = FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MAX),
        NodeIdentity::new(NonZeroU64::new(node).ok_or(Error::Cache)?),
        ContentRevision::new(NonZeroU64::MIN),
    );
    let mut last = CacheKey::new(identity, FilePageIndex::new(0));
    for index in 0..pages {
        last = CacheKey::new(identity, FilePageIndex::new(index));
        let CacheAccess::Load(reservation) = cache.access(last).map_err(|_| Error::Cache)? else {
            return Err(Error::Cache);
        };
        drop(
            reservation
                .fill_and_publish(
                    || crate::kernel::vfs::CachePage::try_copy(&CONTENTS, record.clone()),
                    |_| Refill::Recreate,
                )
                .map_err(|_| Error::Cache)?,
        );
    }
    Ok(last)
}

fn scheduled_domain_reclaim(
    cache: &FileDataCache<crate::kernel::vfs::CachePage>,
    domain: &ResourceDomain,
) -> Result<(), Error> {
    use crate::kernel::mm::reclaim::{Target, reclaim};

    cache.maintain_capacity(256).map_err(|_| Error::Cache)?;
    let child = domain
        .try_new_child(ResourceLimits::UNLIMITED)
        .map_err(|_| Error::Allocation)?;
    let unrelated =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Allocation)?;
    let before = child.usage().committed(ResourceKind::KernelMemoryBytes);
    let related = fill_request_pages(cache, &child, 3, 130)?;
    let other = fill_request_pages(cache, &unrelated, 4, 1)?;
    let weak = cache
        .lookup(related)
        .map_err(|_| Error::Cache)?
        .ok_or(Error::Cache)?
        .value()
        .owner()
        .downgrade();
    let guard = reclaim(Target::Domain(domain.id())).ok_or(Error::Progress)?;
    // One request crosses multiple 64-slot batches and matches the charged
    // ancestor, while an unrelated domain's cached identity remains resident.
    if cache.usage().clean != 1
        || weak.upgrade().is_some()
        || child.usage().committed(ResourceKind::KernelMemoryBytes) != before
        || cache.lookup(other).map_err(|_| Error::Cache)?.is_none()
    {
        return Err(Error::IdentityLifetime);
    }
    if !cache.usage().admission_paused
        || !matches!(cache.access(related), Ok(CacheAccess::CapacityBusy))
    {
        return Err(Error::Cache);
    }
    // The real allocation retry runs while optional cache admission is still
    // paused, after the request serializer has already been released.
    let retry =
        crate::kernel::mm::page_block::PageBlock::allocate(0).map_err(|_| Error::Allocation)?;
    drop(retry);
    drop(guard);
    cache.reclaim_batch(64);
    super::support::quiesce_workers().map_err(|_| Error::Quiescence)?;
    Ok(())
}

fn scheduled_impossible_order(
    cache: &FileDataCache<crate::kernel::vfs::CachePage>,
    domain: &ResourceDomain,
) -> Result<(), Error> {
    use crate::kernel::mm::{
        cache_memory,
        reclaim::{Target, reclaim},
    };

    let memory = cache_memory::availability().ok_or(Error::PhysicalPages)?;
    // The usual 1 GiB QEMU machine cannot form an order-18 block after boot
    // reservations. Larger machines need a separately controlled allocator
    // fixture; do not consume their RAM merely to force this failure shape.
    if memory.managed_pages >= 1_usize << hyper::mm::MAX_ORDER {
        return Ok(());
    }
    cache.maintain_capacity(256).map_err(|_| Error::Cache)?;
    let last = fill_request_pages(cache, domain, 5, 130)?;
    let pinned = cache
        .lookup(last)
        .map_err(|_| Error::Cache)?
        .ok_or(Error::Cache)?;
    let weak = pinned.value().owner().downgrade();
    let memory = cache_memory::availability().ok_or(Error::PhysicalPages)?;
    if memory.available_for_cache() <= cache_memory::watermarks(memory.managed_pages).resume_pages {
        return Err(Error::PhysicalPages);
    }
    let guard = reclaim(Target::PhysicalOrder(hyper::mm::MAX_ORDER)).ok_or(Error::Progress)?;
    // Percentage pressure would do nothing here. The explicit demand must
    // finish its entire finite sweep even though one pin cannot be reclaimed
    // and the requested buddy order can never exist on this machine.
    if cache.usage().clean != 0
        || cache.usage().live_payloads != 1
        || pinned.value().bytes() != CONTENTS
        || weak.upgrade().is_none()
        || !cache.usage().admission_paused
    {
        return Err(Error::PinnedBytes);
    }
    if crate::kernel::mm::page_block::PageBlock::allocate(hyper::mm::MAX_ORDER).is_ok() {
        return Err(Error::PhysicalPages);
    }
    drop(pinned);
    if weak.upgrade().is_some() {
        return Err(Error::IdentityLifetime);
    }
    drop(guard);
    super::support::quiesce_workers().map_err(|_| Error::Quiescence)?;
    Ok(())
}

fn late_publication_under_pressure(
    cache: &FileDataCache<crate::kernel::vfs::CachePage>,
    domain: &ResourceDomain,
) -> Result<(), Error> {
    let record = crate::kernel::vfs::cache_test_record(2, domain).map_err(|_| Error::Allocation)?;
    let identity = FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MAX),
        NodeIdentity::new(NonZeroU64::new(2).ok_or(Error::Cache)?),
        ContentRevision::new(NonZeroU64::MIN),
    );
    // Fill the worker's minimum table, then retain one exclusive eviction as
    // an unfinished loader. Its eventual refill needs no fresh backing/header
    // allocation, whose metadata-reservation release would itself wake the
    // worker and hide a missing load-completion notification.
    for index in 0..64 {
        let key = CacheKey::new(identity, FilePageIndex::new(index));
        let CacheAccess::Load(reservation) = cache.access(key).map_err(|_| Error::Cache)? else {
            return Err(Error::Cache);
        };
        let _ = reservation
            .fill_and_publish(
                || crate::kernel::vfs::CachePage::try_copy(&CONTENTS, record.clone()),
                |_| Refill::Recreate,
            )
            .map_err(|_| Error::Cache)?;
    }
    let key = CacheKey::new(identity, FilePageIndex::new(64));
    let CacheAccess::Load(reservation) = cache.access(key).map_err(|_| Error::Cache)? else {
        return Err(Error::Cache);
    };
    let pressure = Pressure::start();
    let service_workers = crate::kernel::log::permanent_worker_count_for_test()
        + crate::kernel::task::scheduler::permanent_worker_count_for_test()
        + worker::permanent_worker_count_for_test()
        + crate::kernel::vfs::read_ahead::permanent_worker_count_for_test()
        + crate::kernel::device::iommu::runtime::permanent_worker_count_for_test();
    wait_without_allocation(|| {
        let usage = cache.usage();
        let statistics =
            crate::kernel::task::scheduler::statistics().map_err(|_| Error::Quiescence)?;
        Ok(worker::draining_for_test()
            && usage.capacity == 64
            && usage.clean == 0
            && usage.loading == 1
            && !usage.maintenance_pending
            && statistics.ready == 0
            && statistics.running == 1
            && statistics.blocked == service_workers
            && statistics.retirements_in_progress == 0)
    })?;

    let mut reused = false;
    let pinned = reservation
        .fill_and_publish(
            || Err(CacheError::Allocation),
            |_| {
                reused = true;
                Refill::Ready
            },
        )
        .map_err(|_| Error::Cache)?;
    if !reused {
        return Err(Error::Cache);
    }
    // Keep the reader pin and identity until detachment is observed. Neither
    // this publication nor the observer releases a page or metadata claim, so
    // completion itself must wake the sleeping pressure worker.
    wait_without_allocation(|| {
        let usage = cache.usage();
        Ok(usage.clean == 0 && usage.loading == 0 && !usage.maintenance_pending)
    })?;
    if pinned.value().bytes() != CONTENTS {
        return Err(Error::PinnedBytes);
    }
    drop(pinned);
    drop(record);
    drop(pressure);
    wait(|| !worker::draining_for_test())?;
    super::support::quiesce_workers().map_err(|_| Error::Quiescence)?;
    Ok(())
}

/// Timed sleep creates and retires timer storage, which may independently
/// notify memory release. This regression needs a real deadline and scheduler
/// progress without generating that unrelated allocator wake.
fn wait_without_allocation(
    mut condition: impl FnMut() -> Result<bool, Error>,
) -> Result<(), Error> {
    let deadline =
        crate::kernel::time::deadline_after(crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS)
            .map_err(|_| Error::Progress)?;
    loop {
        if condition()? {
            return Ok(());
        }
        if hyper::hal::timer::deadline_reached(crate::kernel::time::monotonic_ticks(), deadline) {
            return Err(Error::Progress);
        }
        crate::kernel::task::scheduler::yield_now().map_err(|_| Error::Progress)?;
    }
}

fn wait(mut condition: impl FnMut() -> bool) -> Result<(), Error> {
    let completed = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, crate::kernel::task::SleepError>(condition()),
    )
    .map_err(|_| Error::Progress)?;
    if completed {
        Ok(())
    } else {
        Err(Error::Progress)
    }
}
