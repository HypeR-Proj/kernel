// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native memory service calls under borrowed Process authority.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use crate::abi::native::MemoryServices;
use crate::capability::HandleValue;
use crate::mm::user_space::UserSlice;

impl MemoryServices for ProcessServices<'_> {
    fn create_contiguous_vmo(
        &self,
        size: u64,
    ) -> Result<HandleValue, crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::create_contiguous_vmo(self.process, size)
    }

    fn create_vmo(
        &self,
        size: u64,
    ) -> Result<HandleValue, crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::create_vmo(self.process, size)
    }

    fn create_vmo_snapshot(
        &self,
        value: &InvocationReceiver,
    ) -> Result<(HandleValue, u64), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::create_vmo_snapshot(self.process, value)
    }
    fn map_file(
        &self,
        vmar: &InvocationReceiver,
        file: HandleValue,
        protection: HandleValue,
        request: crate::mm::user_space::PrivateMappingRequest,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::map_file(self.process, vmar, file, protection, request)
    }
    fn map_private(
        &self,
        vmar: &InvocationReceiver,
        snapshot: HandleValue,
        request: crate::mm::user_space::PrivateMappingRequest,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::map_private(self.process, vmar, snapshot, request)
    }
    fn read_vmo(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        output: Option<UserSlice>,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::read_vmo(self.process, vmo, offset, output)
    }

    fn write_vmo(
        &self,
        vmo: &InvocationReceiver,
        offset: u64,
        input: Option<UserSlice>,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::write_vmo(self.process, vmo, offset, input)
    }

    fn set_memory_residency(
        &self,
        vmar: &InvocationReceiver,
        address: u64,
        size: u64,
        current: bool,
        lock: bool,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::set_memory_residency(
            self.process,
            vmar,
            address,
            size,
            current,
            lock,
        )
    }
    fn allocate_vmar(
        &self,
        parent: &InvocationReceiver,
        address: u64,
        size: u64,
        exact: bool,
    ) -> Result<(HandleValue, u64), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::allocate_vmar(self.process, parent, address, size, exact)
    }

    fn map_vmo(
        &self,
        vmar: &InvocationReceiver,
        vmo: HandleValue,
        vmo_offset: u64,
        address: u64,
        size: u64,
        permissions: crate::mm::user_space::Permissions,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::map_vmo(
            self.process,
            vmar,
            vmo,
            vmo_offset,
            address,
            size,
            permissions,
        )
    }

    fn protect_vmar(
        &self,
        vmar: &InvocationReceiver,
        address: u64,
        size: u64,
        permissions: crate::mm::user_space::Permissions,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::protect(self.process, vmar, address, size, permissions)
    }

    fn unmap_vmar(
        &self,
        vmar: &InvocationReceiver,
        address: u64,
        size: u64,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::unmap(self.process, vmar, address, size)
    }

    fn destroy_vmar(
        &self,
        vmar: &InvocationReceiver,
    ) -> Result<(), crate::mm::user_space::MemoryServiceError> {
        crate::mm::user_space::destroy_vmar(self.process, vmar)
    }
}
