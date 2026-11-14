// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Atomic observation of canonical shared-file storage. Waits retain a page
//! pin and an independent extent lease, never a writer or snapshot admission.

use alloc::boxed::Box;
use core::sync::atomic::Ordering;
use hyper::mm::FallibleArc;

use crate::accounting::ResourceDomain;
use crate::mm::user_space::SyncBinding;
use crate::object::{ObjectWaitError, PreparedSignalObservation, RegisteredSignalObservation};
use crate::task::WaitTicket;

use super::super::{Error, FileObject, remote::Health};
use super::{PAGE, SharedFile, SharedFileSource, SharedPin};

/// The caller charges the sleeping word's pin and wait metadata. This owner
/// retains independently charged canonical storage and an active file lease.
pub(crate) struct FileAtomicWord {
    physical: u64,
    key: (u64, u64, u64),
    health: FallibleArc<Health>,
    _pin: SharedPin,
    _source: SharedFileSource,
}

impl FileAtomicWord {
    pub(crate) fn physical(&self) -> u64 {
        self.physical
    }
    pub(crate) fn key(&self) -> (u64, u64, u64) {
        self.key
    }
    pub(crate) fn prepare_failure_wait(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<PreparedFileFailure, ObjectWaitError> {
        PreparedFileFailure::prepare(self.health.clone(), sponsor)
    }
}

pub(crate) struct PreparedFileFailure {
    health: FallibleArc<Health>,
    peer: PreparedSignalObservation,
    endpoint: PreparedSignalObservation,
    local: PreparedSignalObservation,
}
pub(crate) struct RegisteredFileFailure {
    health: FallibleArc<Health>,
    peer: RegisteredSignalObservation,
    endpoint: RegisteredSignalObservation,
    local: RegisteredSignalObservation,
}

impl PreparedFileFailure {
    fn prepare(
        health: FallibleArc<Health>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, ObjectWaitError> {
        Ok(Self {
            health,
            peer: PreparedSignalObservation::new(sponsor, crate::ipc::ByteChannel::PEER_CLOSED)?,
            endpoint: PreparedSignalObservation::new(
                sponsor,
                crate::ipc::ByteChannel::PEER_CLOSED,
            )?,
            local: PreparedSignalObservation::new(sponsor, Health::FAILED)?,
        })
    }
    pub(crate) fn register(self, ticket: WaitTicket) -> RegisteredFileFailure {
        let [peer, endpoint, local] = self.health.failure_sources();
        let peer = self.peer.register(peer, ticket);
        let endpoint = self.endpoint.register(endpoint, ticket);
        let local = self.local.register(local, ticket);
        RegisteredFileFailure {
            health: self.health,
            peer,
            endpoint,
            local,
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) struct FileFailureTestControl {
    health: FallibleArc<Health>,
}
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
impl FileFailureTestControl {
    pub(crate) fn new(
        channel: crate::object::KernelRef<crate::ipc::ByteChannel, crate::object::KernelService>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        Ok(Self {
            health: Health::try_new(channel, sponsor)?,
        })
    }
    pub(crate) fn prepare(
        &self,
        sponsor: &ResourceDomain,
    ) -> Result<PreparedFileFailure, ObjectWaitError> {
        PreparedFileFailure::prepare(self.health.clone(), sponsor)
    }
    pub(crate) fn fail(&self) {
        self.health.fail();
    }
}
impl RegisteredFileFailure {
    pub(crate) fn finish(self) -> bool {
        let [peer, endpoint, local] = self.health.failure_sources();
        // Always retire all sources, including losers. A live failure level
        // is not a winning result unless this exact ticket observed it.
        let peer = self.peer.finish(peer);
        let endpoint = self.endpoint.finish(endpoint);
        let local = self.local.finish(local);
        peer || endpoint || local
    }
}

fn word_end(offset: u64, width: u64) -> Result<u64, Error> {
    if !matches!(width, 4 | 8) || !offset.is_multiple_of(width) {
        return Err(Error::InvalidInput);
    }
    offset.checked_add(width).ok_or(Error::InvalidInput)
}

fn key(
    file: &SharedFile,
    domain: Option<u64>,
    offset: u64,
    width: u64,
) -> Result<(u64, u64, u64), Error> {
    let end = word_end(offset, width)?;
    let shared_extent = file.size.load(Ordering::Acquire) / PAGE as u64 * PAGE as u64;
    if end > shared_extent
        || domain.is_some_and(|domain| {
            width != 8
                || !file
                    .sync_bindings
                    .with(|bindings| bindings.contains(domain, offset, end))
        })
    {
        return Err(Error::InvalidInput);
    }
    Ok((file.identity.filesystem, file.identity.node, offset))
}

impl FileObject {
    /// Bindings retain only their domain/accounting owner. They deliberately do
    /// not keep a namespace lease, mapping, or resident page alive forever.
    /// A false result denotes overlap attributed to another domain.
    pub(crate) fn bind_shared_sync(
        &self,
        binding: Box<SyncBinding>,
        sponsor: &ResourceDomain,
    ) -> Result<bool, Error> {
        if binding.end <= binding.offset
            || !binding.offset.is_multiple_of(8)
            || !binding.end.is_multiple_of(8)
        {
            return Err(Error::InvalidInput);
        }
        let start = binding.offset / PAGE as u64 * PAGE as u64;
        let end = binding
            .end
            .checked_add(PAGE as u64 - 1)
            .ok_or(Error::InvalidInput)?
            / PAGE as u64
            * PAGE as u64;
        let source =
            SharedFileSource::acquire(self.location().clone(), start, end - start, true, sponsor)?;
        let result = source
            .lease
            .file
            .sync_bindings
            .with(|bindings| bindings.insert(binding));
        // Neither redundant ownership nor a rejected domain may be destroyed
        // while holding the binding lock.
        match result {
            Ok(redundant) => {
                drop(redundant);
                Ok(true)
            }
            Err(rejected) => {
                drop(rejected);
                Ok(false)
            }
        }
    }

    /// Identity-only wake admission. Never promotes storage, fetches metadata,
    /// or populates a page. An unpromoted file has no shared atomic wait domain.
    pub(crate) fn shared_atomic_key(
        &self,
        domain: Option<u64>,
        offset: u64,
        width: u64,
    ) -> Result<(u64, u64, u64), Error> {
        word_end(offset, width)?;
        let location = self.location();
        let content = location.node().content().lock()?;
        location.mount().filesystem().read_status()?;
        let file = content.shared().ok_or(Error::InvalidInput)?;
        key(&file, domain, offset, width)
    }

    pub(crate) fn pin_shared_atomic(
        &self,
        domain: Option<u64>,
        offset: u64,
        width: u64,
        sponsor: &ResourceDomain,
    ) -> Result<FileAtomicWord, Error> {
        word_end(offset, width)?;
        let start = offset / PAGE as u64 * PAGE as u64;
        // The full page requirement is the existing MM01 shared-map contract.
        // This operation lease keeps size and mount identity stable after the
        // originating File capability or userspace mapping is closed. Its
        // potential-write lease excludes executable protection, but is not a
        // short page writer admission and does not prevent ordinary sync.
        let source =
            SharedFileSource::acquire(self.location().clone(), start, PAGE as u64, true, sponsor)?;
        let file = &source.lease.file;
        let key = key(file, domain, offset, width)?;
        let filesystem = file.filesystem.upgrade().ok_or(Error::Missing)?;
        let health = filesystem.atomic_health();
        let page = source.page(start)?;
        let pin = page.try_pin_atomic()?;
        let physical = page
            .physical_address()
            .get()
            .checked_add(offset - start)
            .ok_or(Error::InvalidInput)?;
        Ok(FileAtomicWord {
            physical,
            key,
            health,
            _pin: pin,
            _source: source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_geometry_rejects_bad_width_alignment_and_overflow() {
        assert_eq!(word_end(4092, 4).ok(), Some(4096));
        assert_eq!(word_end(4088, 8).ok(), Some(4096));
        assert!(word_end(1, 4).is_err());
        assert!(word_end(4, 8).is_err());
        assert!(word_end(0, 0).is_err());
        assert!(word_end(0, 16).is_err());
        assert!(word_end(u64::MAX - 7, 8).is_err());
    }
}
