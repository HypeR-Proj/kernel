// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! User-memory transfer and address-space service contracts.

use crate::capability::{HandleValue, InvocationReceiver};
use crate::mm::user_space::{MemoryServiceError, Permissions, UserSlice};
use crate::process::ProcessError;

pub(crate) trait UserMemoryServices {
    fn copy_to_user(&self, destination: UserSlice, source: &[u8]) -> Result<(), ProcessError>;
    fn copy_from_user(&self, source: UserSlice, destination: &mut [u8])
    -> Result<(), ProcessError>;
}

pub(crate) trait MemoryServices {
    fn executable_publish(
        &self,
        authority: &InvocationReceiver,
        source: HandleValue,
    ) -> Result<(HandleValue, u64), MemoryServiceError>;
    fn vmo_wait_u32(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        expected: u32,
        deadline: u64,
    ) -> Result<crate::task::WaitOutcome, MemoryServiceError>;
    fn vmo_wake_u32(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        count: u32,
    ) -> Result<u64, MemoryServiceError>;
    fn create_contiguous_vmo(&self, size: u64) -> Result<HandleValue, MemoryServiceError>;
    fn create_vmo(&self, size: u64) -> Result<HandleValue, MemoryServiceError>;
    fn create_vmo_snapshot(
        &self,
        vmo: &InvocationReceiver,
    ) -> Result<(HandleValue, u64), MemoryServiceError>;
    fn map_file(
        &self,
        vmar: &InvocationReceiver,
        file: HandleValue,
        protection: HandleValue,
        request: crate::mm::user_space::PrivateMappingRequest,
    ) -> Result<(), MemoryServiceError>;
    fn map_private(
        &self,
        vmar: &InvocationReceiver,
        snapshot: HandleValue,
        request: crate::mm::user_space::PrivateMappingRequest,
    ) -> Result<(), MemoryServiceError>;
    fn read_vmo(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        output: Option<UserSlice>,
    ) -> Result<(), MemoryServiceError>;
    fn write_vmo(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        input: Option<UserSlice>,
    ) -> Result<(), MemoryServiceError>;
    #[allow(clippy::too_many_arguments)]
    fn map_vmo(
        &self,
        vmar: &InvocationReceiver,
        vmo: HandleValue,
        vmo_offset: u64,
        address: u64,
        size: u64,
        permissions: Permissions,
    ) -> Result<(), MemoryServiceError>;
    fn set_memory_residency(
        &self,
        vmar: &InvocationReceiver,
        scope: crate::mm::user_space::ResidencyScope,
        action: crate::mm::user_space::ResidencyAction,
    ) -> Result<(), MemoryServiceError>;
    fn allocate_vmar(
        &self,
        parent: &InvocationReceiver,
        address: u64,
        size: u64,
        exact: bool,
    ) -> Result<(HandleValue, u64), MemoryServiceError>;
    fn protect_vmar(
        &self,
        vmar: &InvocationReceiver,
        address: u64,
        size: u64,
        permissions: Permissions,
    ) -> Result<(), MemoryServiceError>;
    fn unmap_vmar(
        &self,
        vmar: &InvocationReceiver,
        address: u64,
        size: u64,
    ) -> Result<(), MemoryServiceError>;
    fn destroy_vmar(&self, vmar: &InvocationReceiver) -> Result<(), MemoryServiceError>;
}
