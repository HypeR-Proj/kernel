// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! File-owned mutable frames and short admissions used by mapped aliases.

use alloc::boxed::Box;
use hyper::mm::{PhysicalAddress, WeakFallibleArc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SharedFileError {
    Busy,
    Unavailable,
    Allocation,
    InvalidRange,
    Unsupported,
}

/// Reverse edges are weak. The mapping owns this callback, never the file.
pub(crate) trait SharedAlias: Send + Sync {
    /// Runs outside file/cache locks with page admission closed until return.
    /// The unique page identity and cut epoch cannot be reused while this
    /// synchronous callback is in flight. Success includes translation retirement.
    fn withdraw(&self, page_id: u64, epoch: u64, all: bool) -> Result<(), ()>;
}

pub(crate) trait SharedAdmission {
    fn epoch(&self) -> u64;
    fn valid(&self) -> bool;
}

pub(crate) trait SharedPage: Clone + Send + Sync {
    type Mutation: SharedAdmission;
    type Install: SharedAdmission;
    type Pin;

    fn identity(&self) -> u64;
    fn epoch(&self) -> u64;
    fn physical_address(&self) -> PhysicalAddress;
    fn try_pin(&self) -> Result<Self::Pin, SharedFileError>;
    fn try_begin_write(&self) -> Result<Self::Mutation, SharedFileError>;
    fn try_begin_install(&self) -> Result<Self::Install, SharedFileError>;
    fn register_alias(
        &self,
        alias: WeakFallibleArc<Box<dyn SharedAlias>>,
    ) -> Result<(), SharedFileError>;
    fn read_exposed(&self, offset: usize, bytes: &mut [u8]) -> Result<(), SharedFileError>;
    fn write_exposed(
        &self,
        admission: &Self::Mutation,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), SharedFileError>;
}

pub(crate) trait SharedSource: Clone + Send + Sync {
    type Page: SharedPage;

    fn identity(&self) -> (u64, u64);
    fn size(&self) -> u64;
    fn page(&self, offset: u64) -> Result<Self::Page, SharedFileError>;
    /// Retains independent node/mount authority for synchronization or a kernel
    /// copy. Explicit mapping retirement must not invalidate this operation.
    fn retain_sync(&self) -> Result<Self, SharedFileError>;
    fn sync(&self, scope: u32) -> Result<(), SharedFileError>;
    /// Explicit logical retirement; old observation owners confer no authority.
    fn retire(&self);
}
