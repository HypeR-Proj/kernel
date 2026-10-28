// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared content identity without an active namespace lease.

use hyper::mm::FallibleArc;

use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceDomainId,
    crate::accounting::ResourceKind,
};

use super::file_data::FileContent;
use super::instance::Error;

pub(crate) type CachePage = crate::fs::cache::FilePage<FallibleArc<FileRecord>>;
/// One bounded read whose immutable page pins outlive the content gate.
pub(super) type ReadPages = crate::fs::cache::PageRead<FallibleArc<FileRecord>>;

/// Active nodes and cached pages share this exact content incarnation.
///
/// Cache ownership confers no namespace lease or file authority. In particular,
/// advisory locks, mount pins, paths, and backend owners stay on the active
/// node or its namespace binding.
///
/// Emergency page reclaim may release the final record outside allocator and
/// cache locks, while its caller holds unrelated kernel locks. Keep destruction
/// limited to quiescent scalar/mutex storage, atomic quota release, and ordinary
/// deallocation. Adding callbacks or other owners requires re-auditing that
/// contract. Demand content guards retain an active node; speculative guards
/// retain this record alone and do not extend namespace-lease lifetime.
pub(crate) struct FileRecord {
    id: u64,
    pub(super) content: FileContent,
    _charge: CommittedCharge,
}

impl FileRecord {
    pub(super) fn try_new(id: u64, domain: &ResourceDomain) -> Result<FallibleArc<Self>, Error> {
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                FallibleArc::<Self>::allocation_size() as u64,
            ))
            .map_err(Error::Resource)?
            .commit();
        FallibleArc::try_new(Self {
            id,
            content: FileContent::new(),
            _charge: charge,
        })
        .map_err(Error::from)
    }

    pub(super) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn charges_domain(&self, domain: ResourceDomainId) -> bool {
        self._charge.charges_domain(domain)
    }
}
