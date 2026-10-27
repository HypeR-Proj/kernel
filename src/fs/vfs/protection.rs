// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Process-owned protection of file contents, independent of File handles.

use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;

use crate::authority::Rights;
use crate::fs::cache::FileDataCache;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::TransferClass, crate::object::object_allocation_size, crate::object::private,
};

use super::file_record::CachePage;
use super::instance::Location;
use super::{Error, FileObject};

struct ProtectedContent {
    location: Location,
    cache: FallibleArc<FileDataCache<CachePage>>,
    size: u64,
    _charge: CommittedCharge,
}

impl Drop for ProtectedContent {
    fn drop(&mut self) {
        self.location.node().content().release_protection();
    }
}

/// A stable file identity and length, without retaining any file-data pages.
///
/// Acquiring a guard waits for the current content operation. While any guard
/// remains, all subsequent writes and truncations through any open or alias
/// fail with Busy. Kernel mappings may clone the guard; ordinary File handles
/// never contain one. The last owner releases protection without blocking.
#[derive(Clone)]
pub(crate) struct FileProtection {
    content: FallibleArc<ProtectedContent>,
}

impl FileProtection {
    pub(super) fn acquire(
        location: Location,
        cache: FallibleArc<FileDataCache<CachePage>>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let charge = sponsor
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                FallibleArc::<ProtectedContent>::allocation_size() as u64,
            ))?
            .commit();
        let size = {
            let mut content = location.node().content().lock()?;
            let filesystem = location.mount().filesystem();
            filesystem.read_status()?;
            let size = content.length(|| {
                filesystem
                    .attributes(location.node())
                    .map(|attributes| attributes.size())
            })?;
            // This is the commit point, serialized with every content writer.
            // Allocation failure below drops ProtectedContent and rolls back
            // the count before returning; no provisional token is published.
            content.acquire_protection()?;
            size
        };
        let content = FallibleArc::try_new(ProtectedContent {
            location,
            cache,
            size,
            _charge: charge,
        })
        .map_err(|_| Error::Allocation)?;
        Ok(Self { content })
    }

    pub(crate) fn size(&self) -> u64 {
        self.content.size
    }

    /// Read ordinary file data while retaining content stability. Backing
    /// cache entries remain reclaimable and a backend failure is still an
    /// error: protection never turns a failed mount into a readable snapshot.
    pub(crate) fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, Error> {
        let content = &self.content;
        content
            .location
            .mount()
            .filesystem()
            .read_file_observed(content.location.node(), &content.cache, offset, output)
            .map(|(bytes, _)| bytes)
            .map_err(Into::into)
    }

    pub(crate) fn shared_page(
        &self,
        offset: u64,
    ) -> Result<Option<crate::fs::cache::MappedPage<CachePage>>, Error> {
        let content = &self.content;
        content
            .location
            .mount()
            .filesystem()
            .shared_file_page(content.location.node(), &content.cache, offset)
            .map_err(Into::into)
    }

    pub(crate) fn matches_file(&self, file: &FileObject) -> bool {
        // Bind mounts and independently opened hard links must agree. Comparing
        // the canonical content gate avoids treating a pathname or mount ID as
        // content identity, and live Location owners exclude identity reuse.
        core::ptr::eq(
            self.content.location.node().content(),
            file.location().node().content(),
        )
    }
}

/// A nontransferable process-local token for one protection acquisition.
///
/// A process owns the active token through its handle table, and its private
/// mappings own any additional guard references. Handle teardown retires the
/// token even if an inspection or operation pin keeps this object allocated.
/// Mapping services must reject VMARs belonging to a different process.
pub(crate) struct FileProtectionObject {
    protection: InterruptSpinLock<Option<FileProtection>, crate::hal::irq::LocalMask>,
    _charge: CommittedCharge,
}

impl FileProtectionObject {
    pub(crate) fn try_new(
        protection: FileProtection,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let bytes = object_allocation_size::<Self>()
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(Error::AllocationSize)?;
        let charge = sponsor
            .reserve(
                ResourceAmount::ZERO
                    .with(ResourceKind::KernelObjects, 1)
                    .with(ResourceKind::KernelMemoryBytes, bytes),
            )?
            .commit();
        Ok(Self {
            protection: InterruptSpinLock::new(Some(protection)),
            _charge: charge,
        })
    }

    pub(crate) fn protection_for(&self, file: &FileObject) -> Result<FileProtection, Error> {
        self.protection.with(|protection| {
            let protection = protection.as_ref().ok_or(Error::Busy)?;
            if !protection.matches_file(file) {
                return Err(Error::InvalidInput);
            }
            Ok(protection.clone())
        })
    }
}

impl private::Sealed for FileProtectionObject {}
impl private::UserExportable for FileProtectionObject {}

impl KernelObject for FileProtectionObject {
    const KIND: ObjectKind = ObjectKind::FILE_PROTECTION;
    const SUPPORTED_RIGHTS: Rights = Rights::MAP.union(Rights::INSPECT);
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;

    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        let retired = self.protection.with(Option::take);
        drop(retired);
    }
}
