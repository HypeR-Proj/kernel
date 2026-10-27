// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stable file identity retained without retaining any resident file pages.

use super::{MemoryAccount, MemoryCharge, PageBackend, UserMemoryLock, VmoError, map_allocation};
use alloc::boxed::Box;
use hyper::mm::FallibleArc;

/// A content-protected file. Reads may block and must run outside MM locks and
/// CPU pins. Implementations retain protection, not a whole-file memory copy.
pub(crate) trait FilePageSource<B: PageBackend>: Send + Sync {
    /// Returns canonical immutable cache storage for a complete file page.
    /// None means this backend has no shared cache (for example boot RAMFS).
    fn shared_page(&self, _offset: u64) -> Result<Option<B::Page>, ()> {
        Ok(None)
    }
    fn size(&self) -> u64;
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize, ()>;
}

pub(super) struct FileBacking<B: PageBackend, A: MemoryAccount> {
    source: UserMemoryLock<Option<FallibleArc<Box<dyn FilePageSource<B>>>>>,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> FileBacking<B, A> {
    pub(super) fn try_new<E>(
        source: Box<dyn FilePageSource<B>>,
        account: &A,
    ) -> Result<FallibleArc<Self>, VmoError<E, A::Error>> {
        let charge = account
            .try_charge(MemoryCharge {
                kernel_bytes: (FallibleArc::<Self>::allocation_size()
                    + core::mem::size_of_val(&*source)
                    + FallibleArc::<Box<dyn FilePageSource<B>>>::allocation_size())
                    as u64,
                ..MemoryCharge::default()
            })
            .map_err(VmoError::Account)?;
        let source = FallibleArc::try_new(source).map_err(map_allocation)?;
        FallibleArc::try_new(Self {
            source: UserMemoryLock::new(Some(source)),
            _charge: charge,
        })
        .map_err(map_allocation)
    }

    pub(super) fn shared_page(&self, offset: u64) -> Result<Option<B::Page>, ()> {
        let source = self.source.with(|source| source.clone()).ok_or(())?;
        source.shared_page(offset)
    }

    pub(super) fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize, ()> {
        let source = self.source.with(|source| source.clone()).ok_or(())?;
        source.read_at(offset, output)
    }

    /// Thread retirement closes file-backed refill without needing a new root
    /// or any allocation. Escaped VMAR owners may keep the dead address space
    /// allocated, but cannot keep its file protection active.
    pub(super) fn retire(&self) {
        let source = self.source.with(Option::take);
        drop(source);
    }
}
