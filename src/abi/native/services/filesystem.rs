// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Directory and file service contracts.

use super::memory::UserMemoryServices;
use crate::capability::{HandleValue, InvocationReceiver, Rights};
use crate::fs::vfs::{DirectoryInfo, DirectoryPage, FileInfo, VfsServiceError};
use crate::mm::user_space::UserSlice;

pub(crate) trait VfsServices: UserMemoryServices {
    fn mount_ramfs(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
    ) -> Result<(), VfsServiceError>;
    fn switch_root(
        &self,
        directory: &InvocationReceiver,
        request: &[u8; 48],
    ) -> Result<(), VfsServiceError>;

    fn begin_root_switch(
        &self,
        directory: &InvocationReceiver,
        request: &[u8; 48],
    ) -> Result<HandleValue, VfsServiceError>;
    fn finish_root_switch(
        &self,
        token: &InvocationReceiver,
        commit: bool,
    ) -> Result<(), VfsServiceError>;
    fn directory_scope_create(
        &self,
        root: &InvocationReceiver,
        start: HandleValue,
        rights: Rights,
    ) -> Result<HandleValue, VfsServiceError>;
    fn directory_get_metadata(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        follow: bool,
    ) -> Result<crate::fs::vfs::Metadata, VfsServiceError>;
    fn file_get_metadata(
        &self,
        file: &InvocationReceiver,
    ) -> Result<crate::fs::vfs::Metadata, VfsServiceError>;
    fn directory_get_self_metadata(
        &self,
        directory: &InvocationReceiver,
    ) -> Result<crate::fs::vfs::Metadata, VfsServiceError>;
    fn directory_set_metadata(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        follow: bool,
        update: crate::fs::vfs::MetadataUpdate,
    ) -> Result<(), VfsServiceError>;
    fn file_set_metadata(
        &self,
        file: &InvocationReceiver,
        update: crate::fs::vfs::MetadataUpdate,
    ) -> Result<(), VfsServiceError>;
    fn directory_rename(
        &self,
        source: &InvocationReceiver,
        path: UserSlice,
        destination: HandleValue,
        new_path: UserSlice,
    ) -> Result<(), VfsServiceError>;
    fn directory_link(
        &self,
        source: &InvocationReceiver,
        path: UserSlice,
        destination: HandleValue,
        new_path: UserSlice,
    ) -> Result<(), VfsServiceError>;
    fn directory_symlink(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        target: UserSlice,
    ) -> Result<(), VfsServiceError>;
    fn directory_read_link(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
    ) -> Result<crate::fs::vfs::ScratchVec<u8>, VfsServiceError>;
    fn directory_canonicalize(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
    ) -> Result<crate::fs::vfs::ScratchString, VfsServiceError>;
    fn directory_remove_if(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        is_directory: bool,
        expected: u64,
    ) -> Result<(), VfsServiceError>;
    fn directory_open_directory_nofollow(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        rights: Rights,
    ) -> Result<HandleValue, VfsServiceError>;
    fn file_sync(&self, file: &InvocationReceiver, scope: u64) -> Result<(), VfsServiceError>;
    fn file_lock(
        &self,
        file: &InvocationReceiver,
        mode: crate::fs::vfs::locks::LockMode,
        deadline: u64,
    ) -> Result<(), VfsServiceError>;
    fn file_unlock(&self, file: &InvocationReceiver) -> Result<(), VfsServiceError>;
    fn directory_open_file_with_options(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        rights: Rights,
        options: u64,
        mode: u32,
    ) -> Result<HandleValue, VfsServiceError>;

    fn create_directory(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        mode: u32,
    ) -> Result<(), VfsServiceError>;
    fn remove_entry(
        &self,
        directory: &InvocationReceiver,
        path: UserSlice,
        is_directory: bool,
    ) -> Result<(), VfsServiceError>;
    fn resize_file(&self, file: &InvocationReceiver, length: u64) -> Result<(), VfsServiceError>;
    fn write_file_at(
        &self,
        file: &InvocationReceiver,
        offset: Option<u64>,
        input: Option<UserSlice>,
    ) -> Result<(u64, u64), VfsServiceError>;

    fn open_directory(
        &self,
        root: &InvocationReceiver,
        path: UserSlice,
        rights: Rights,
    ) -> Result<HandleValue, VfsServiceError>;
    fn begin_directory_read(
        &self,
        directory: &InvocationReceiver,
    ) -> Result<HandleValue, VfsServiceError>;
    fn read_directory(
        &self,
        directory: &InvocationReceiver,
        cookie: u64,
        page: &mut DirectoryPage,
    ) -> Result<(), VfsServiceError>;
    fn read_file_at(
        &self,
        file: &InvocationReceiver,
        offset: u64,
        output: Option<UserSlice>,
    ) -> Result<(u64, u64), VfsServiceError>;
    fn protect_file(
        &self,
        file: &InvocationReceiver,
    ) -> Result<(HandleValue, u64), VfsServiceError>;
    fn prefetch_file(
        &self,
        file: &InvocationReceiver,
        offset: u64,
        length: u64,
    ) -> Result<u64, VfsServiceError>;
    fn file_info(&self, file: &InvocationReceiver) -> Result<FileInfo, VfsServiceError>;
    fn directory_info(
        &self,
        directory: &InvocationReceiver,
    ) -> Result<DirectoryInfo, VfsServiceError>;
}
