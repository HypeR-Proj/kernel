// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stable externally mutable frames. Never lend Rust slices over these bytes.

use super::state::{Phase, State};
use super::*;
use crate::mm::user_space::SharedAlias;

const MAX_ALIASES: usize = 64;
static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

struct PageState {
    admission: State,
    aliases: Vec<WeakFallibleArc<Box<dyn SharedAlias>>>,
}

struct PageOwner {
    id: u64,
    frame: crate::mm::page_block::PageBlock,
    address: usize,
    state: InterruptSpinLock<PageState, crate::hal::irq::LocalMask>,
    health: WeakFallibleArc<super::super::remote::RemoteFs>,
    _charge: CommittedCharge,
    _residency: FramePermit,
    _record: FallibleArc<FileRecord>,
}

#[derive(Clone)]
pub(crate) struct SharedFilePage(FallibleArc<PageOwner>);

pub(crate) struct SharedPin {
    page: SharedFilePage,
}
impl Drop for SharedPin {
    fn drop(&mut self) {
        self.page.0.state.with(|state| state.admission.unpin());
    }
}

pub(crate) struct SharedMutation {
    page: SharedFilePage,
    epoch: u64,
}
pub(crate) struct SharedInstall {
    page: SharedFilePage,
    epoch: u64,
}

impl SharedMutation {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn valid(&self) -> bool {
        self.page
            .0
            .state
            .with(|s| s.admission.valid(self.epoch, true))
    }
}
impl Drop for SharedMutation {
    fn drop(&mut self) {
        self.page.0.state.with(|s| s.admission.release(true));
    }
}
impl SharedInstall {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn valid(&self) -> bool {
        self.page
            .0
            .state
            .with(|s| s.admission.valid(self.epoch, false))
    }
}
impl Drop for SharedInstall {
    fn drop(&mut self) {
        self.page.0.state.with(|s| s.admission.release(false));
    }
}

impl SharedFilePage {
    pub(super) fn load(
        file: &SharedFile,
        offset: u64,
        residency: FramePermit,
    ) -> Result<Self, Error> {
        let filesystem = file.filesystem.upgrade().ok_or(Error::Missing)?;
        let charge = charge(
            &filesystem.domain,
            FallibleArc::<PageOwner>::allocation_size()
                + MAX_ALIASES * core::mem::size_of::<WeakFallibleArc<Box<dyn SharedAlias>>>(),
        )?;
        let mut aliases = Vec::new();
        aliases
            .try_reserve_exact(MAX_ALIASES)
            .map_err(|_| Error::Allocation)?;
        let frame = crate::mm::cache_memory::allocate_page().map_err(|_| Error::Allocation)?;
        let address =
            crate::mm::memory::linear_address(frame.physical().get()).ok_or(Error::Allocation)?;
        // SAFETY: unpublished frame is exclusively owned and resident. Its
        // entire bytes are initialized before the backend lends this slice.
        let bytes = unsafe {
            let pointer = core::ptr::with_exposed_provenance_mut::<u8>(address);
            pointer.write_bytes(0, PAGE);
            core::slice::from_raw_parts_mut(pointer, PAGE)
        };
        let length =
            (file.size.load(Ordering::Acquire).saturating_sub(offset)).min(PAGE as u64) as usize;
        if filesystem.shared_read(file.identity.node, offset, &mut bytes[..length])? != length {
            return Err(Error::InvalidBackendResult);
        }
        let id = NEXT_PAGE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::IdentifierExhausted)?;
        Ok(Self(FallibleArc::try_new(PageOwner {
            id,
            frame,
            address,
            state: InterruptSpinLock::new(PageState {
                admission: State::new(),
                aliases,
            }),
            health: file.filesystem.clone(),
            _charge: charge,
            _residency: residency,
            _record: file.record.upgrade().ok_or(Error::Missing)?,
        })?))
    }
    pub(crate) fn identity(&self) -> u64 {
        self.0.id
    }
    pub(crate) fn physical_address(&self) -> hyper::mm::PhysicalAddress {
        self.0.frame.physical()
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.0.state.with(|s| s.admission.epoch)
    }
    fn healthy(&self) -> Result<(), Error> {
        self.0.health.upgrade().ok_or(Error::Missing)?.read_status()
    }
    pub(crate) fn try_pin(&self) -> Result<SharedPin, super::super::Error> {
        self.healthy()?;
        if !self.0.state.with(|s| s.admission.pin()) {
            return Err(super::super::Error::Busy);
        }
        Ok(SharedPin { page: self.clone() })
    }
    /// A read-only atomic observation may join an ordinary snapshot freeze;
    /// the snapshot never replaces its canonical frame. Retirement under
    /// pressure still returns Busy rather than exposing a stale cache frame.
    pub(crate) fn try_pin_atomic(&self) -> Result<SharedPin, super::super::Error> {
        self.healthy()?;
        if !self.0.state.with(|s| s.admission.pin_atomic()) {
            return Err(super::super::Error::Busy);
        }
        Ok(SharedPin { page: self.clone() })
    }
    pub(crate) fn try_begin_write(&self) -> Result<SharedMutation, super::super::Error> {
        self.healthy()?;
        let epoch = self
            .0
            .state
            .with(|s| s.admission.admit(true))
            .ok_or(Error::Busy)?;
        Ok(SharedMutation {
            page: self.clone(),
            epoch,
        })
    }
    pub(crate) fn try_begin_install(&self) -> Result<SharedInstall, super::super::Error> {
        self.healthy()?;
        let epoch = self
            .0
            .state
            .with(|s| s.admission.admit(false))
            .ok_or(Error::Busy)?;
        Ok(SharedInstall {
            page: self.clone(),
            epoch,
        })
    }
    pub(crate) fn register_alias(
        &self,
        alias: WeakFallibleArc<Box<dyn SharedAlias>>,
    ) -> Result<(), super::super::Error> {
        self.0
            .state
            .with(|s| {
                if s.admission.phase != Phase::Active {
                    return Err(Error::Busy);
                }
                if s.aliases.iter().any(|old| old.ptr_eq(&alias)) {
                    return Ok(());
                }
                // Weak headers have no payload destructor; pruning is nonblocking.
                s.aliases.retain(WeakFallibleArc::is_alive);
                if s.aliases.len() == MAX_ALIASES {
                    return Err(Error::Allocation);
                }
                s.aliases.push(alias);
                Ok(())
            })
            .map_err(Into::into)
    }
    pub(crate) fn read_exposed(
        &self,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<(), super::super::Error> {
        self.healthy()?;
        self.check_range(offset, bytes.len())?;
        // SAFETY: retained frame and checked private destination are resident
        // and nonoverlapping. This raw external copy promises no atomic image.
        unsafe {
            crate::hal::user::copy_from_exposed(
                core::ptr::with_exposed_provenance::<u8>(self.0.address + offset),
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        }
        .map_err(|_| super::super::Error::Unsupported)
    }
    pub(crate) fn write_exposed(
        &self,
        admission: &SharedMutation,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), super::super::Error> {
        self.check_range(offset, bytes.len())?;
        if admission.page.identity() != self.identity() || !admission.valid() {
            return Err(Error::Busy.into());
        }
        // SAFETY: admission excludes snapshot capture/retirement; the caller's
        // private staging bytes cannot alias this externally mutable frame.
        unsafe {
            crate::hal::user::copy_to_exposed(
                bytes.as_ptr(),
                core::ptr::with_exposed_provenance_mut::<u8>(self.0.address + offset),
                bytes.len(),
            )
        }
        .map_err(|_| super::super::Error::Unsupported)
    }
    fn check_range(&self, offset: usize, length: usize) -> Result<(), Error> {
        if offset.checked_add(length).is_none_or(|end| end > PAGE) {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }
    pub(super) fn dirty(&self) -> bool {
        self.0.state.with(|s| s.admission.dirty)
    }
    pub(super) fn dirty_epoch(&self) -> Option<u64> {
        self.0.state.with(|s| {
            (s.admission.dirty && s.admission.phase == Phase::Active).then_some(s.admission.epoch)
        })
    }
    pub(super) fn try_close_for_reclaim(&self) -> bool {
        self.0
            .state
            .with(|state| state.admission.try_close_for_reclaim())
    }
    pub(super) fn close_for_eviction(&self) -> Option<bool> {
        self.0.state.with(|s| s.admission.close_for_eviction())
    }
    pub(super) fn close(&self) -> bool {
        self.0.state.with(|s| s.admission.close())
    }
    pub(super) fn reopen(&self) {
        self.0.state.with(|s| s.admission.reopen());
    }
    pub(super) fn withdraw(&self, all: bool) -> Result<(), Error> {
        let epoch = self.epoch();
        let mut index = 0;
        loop {
            // No allocation, callback, or strong-owner destruction under the
            // page lock. Registration is closed throughout this walk.
            let alias = self.0.state.with(|s| s.aliases.get(index).cloned());
            let Some(alias) = alias else {
                return Ok(());
            };
            if let Some(alias) = alias.upgrade() {
                alias
                    .withdraw(self.identity(), epoch, all)
                    .map_err(|_| Error::Busy)?;
            }
            index += 1;
        }
    }
    pub(super) fn capture(&self, output: &mut [u8], retire: bool) -> Result<(), Error> {
        self.read_exposed(0, output).map_err(vfs_error)?;
        self.0
            .state
            .with(|s| s.admission.captured(retire))
            .ok_or(Error::IdentifierExhausted)?;
        Ok(())
    }
    pub(super) fn submission_failed(&self) {
        self.0.state.with(|s| s.admission.submission_failed());
    }
    pub(super) fn fail(&self) {
        self.0.state.with(|s| s.admission.phase = Phase::Failed);
    }
}
