// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::mm::user_space::{
    SharedAdmission, SharedAlias, SharedFileError, SharedPage, SharedSource,
};

fn error(error: super::super::Error) -> SharedFileError {
    match error {
        super::super::Error::Busy => SharedFileError::Busy,
        super::super::Error::Allocation | super::super::Error::Resource(_) => {
            SharedFileError::Allocation
        }
        super::super::Error::Unsupported => SharedFileError::Unsupported,
        super::super::Error::InvalidInput | super::super::Error::InvalidSize => {
            SharedFileError::InvalidRange
        }
        _ => SharedFileError::Unavailable,
    }
}
impl SharedAdmission for SharedMutation {
    fn epoch(&self) -> u64 {
        self.epoch()
    }
    fn valid(&self) -> bool {
        self.valid()
    }
}
impl SharedAdmission for SharedInstall {
    fn epoch(&self) -> u64 {
        self.epoch()
    }
    fn valid(&self) -> bool {
        self.valid()
    }
}
impl SharedPage for SharedFilePage {
    type Pin = SharedPin;
    fn try_pin(&self) -> Result<Self::Pin, SharedFileError> {
        self.try_pin().map_err(error)
    }
    type Mutation = SharedMutation;
    type Install = SharedInstall;
    fn identity(&self) -> u64 {
        self.identity()
    }
    fn physical_address(&self) -> hyper::mm::PhysicalAddress {
        self.physical_address()
    }
    fn try_begin_write(&self) -> Result<Self::Mutation, SharedFileError> {
        self.try_begin_write().map_err(error)
    }
    fn try_begin_install(&self) -> Result<Self::Install, SharedFileError> {
        self.try_begin_install().map_err(error)
    }
    fn register_alias(
        &self,
        alias: WeakFallibleArc<Box<dyn SharedAlias>>,
    ) -> Result<(), SharedFileError> {
        self.register_alias(alias).map_err(error)
    }
    fn read_exposed(&self, offset: usize, bytes: &mut [u8]) -> Result<(), SharedFileError> {
        self.read_exposed(offset, bytes).map_err(error)
    }
    fn write_exposed(
        &self,
        admission: &Self::Mutation,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), SharedFileError> {
        self.write_exposed(admission, offset, bytes).map_err(error)
    }
}
impl SharedSource for SharedFileSource {
    type Page = SharedFilePage;
    fn identity(&self) -> (u64, u64) {
        let id = self.identity();
        (id.filesystem, id.node)
    }
    fn size(&self) -> u64 {
        self.size()
    }
    fn page(&self, offset: u64) -> Result<Self::Page, SharedFileError> {
        self.page(offset).map_err(error)
    }
    fn retain_sync(&self) -> Result<Self, SharedFileError> {
        self.retain_sync().map_err(error)
    }
    fn sync(&self, scope: u32) -> Result<(), SharedFileError> {
        self.sync(scope).map_err(error)
    }
    fn retire(&self) {
        self.retire();
    }
}
