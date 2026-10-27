// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Independent directory scans retain positions, never cache pages.

use core::sync::atomic::{AtomicU64, Ordering};

use hyper::mm::FallibleArc;
use hyper_filesystem::protocol::DirectoryCursor;

use crate::authority::Rights;
use crate::sync::Mutex;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

use super::instance::{Location, MountNamespace};
use super::{DirectoryPage, Error};

/// Trusted backend position. None of these fields are supplied by applications.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RemoteDirectoryPosition {
    pub(super) revision: Option<u64>,
    pub(super) checkpoint: DirectoryCursor,
    pub(super) batch_start: DirectoryCursor,
    pub(super) batch_index: usize,
    pub(super) batch_revision: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DirectoryPosition {
    pub(super) ramfs_cookie: u64,
    pub(super) remote: RemoteDirectoryPosition,
}

impl DirectoryPosition {
    fn same_progress(&self, other: &Self) -> bool {
        // Reclaim or attribute changes can discard/rebuild batch hints without
        // changing the backend position at which the next visible item starts.
        self.ramfs_cookie == other.ramfs_cookie
            && self.remote.revision == other.remote.revision
            && self.remote.checkpoint == other.remote.checkpoint
    }
}

static NEXT_COOKIE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct PreviousRead {
    cookie: u64,
    position: DirectoryPosition,
    next_cookie: u64,
}

struct ScanState {
    position: DirectoryPosition,
    cookie: Option<u64>,
    previous: Option<PreviousRead>,
}

/// A handle-owned scan bound to the location observed at `begin_read`.
///
/// Duplicate handles share progress. Creating another stream yields an
/// independent scan, including when both streams originate from one Directory.
/// Only the most recent input cookie can be replayed after failed copyout.
pub(crate) struct DirectoryStream {
    namespace: FallibleArc<MountNamespace>,
    location: Location,
    state: Mutex<ScanState>,
    _charge: CommittedCharge,
}

impl DirectoryStream {
    pub(super) fn try_new(
        namespace: FallibleArc<MountNamespace>,
        location: Location,
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
            namespace,
            location,
            state: Mutex::new(ScanState {
                position: DirectoryPosition::default(),
                cookie: Some(0),
                previous: None,
            }),
            _charge: charge,
        })
    }

    pub(crate) fn read_page(&self, cookie: u64, page: &mut DirectoryPage) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(super::instance::Error::Lock)?;
        let previous = state.previous.filter(|previous| previous.cookie == cookie);
        let position = match previous {
            Some(previous) => previous.position,
            None if state.cookie == Some(cookie) => state.position,
            None => return Err(Error::InvalidDirectoryCookie),
        };
        let next_position = self
            .namespace
            .read_directory_page(&self.location, &position, page)?;
        let more = page.next_cookie != 0;
        if page.len == 0 && more {
            return Err(Error::Backend(super::instance::Error::InvalidBackendResult));
        }
        if let Some(previous) = previous {
            // Refetching is deliberate: copyout retries do not pin directory
            // pages, and must still observe failed backends or stale versions.
            if !next_position.same_progress(&state.position) || more != (previous.next_cookie != 0)
            {
                return Err(Error::InvalidDirectoryCookie);
            }
            page.next_cookie = previous.next_cookie;
            return Ok(());
        }
        let next_cookie = if more {
            NEXT_COOKIE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .map_err(|_| super::instance::Error::IdentifierExhausted)?
        } else {
            0
        };
        state.previous = Some(PreviousRead {
            cookie,
            position,
            next_cookie,
        });
        state.position = next_position;
        state.cookie = (next_cookie != 0).then_some(next_cookie);
        page.next_cookie = next_cookie;
        Ok(())
    }
}

impl private::Sealed for DirectoryStream {}
impl private::UserExportable for DirectoryStream {}
impl KernelObject for DirectoryStream {
    const KIND: ObjectKind = ObjectKind::DIRECTORY_STREAM;
    const SUPPORTED_RIGHTS: Rights = Rights::READ
        .union(Rights::DUPLICATE)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT);
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
}
