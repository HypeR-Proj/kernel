// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Physical device claim and DMA service contracts.

use super::memory::UserMemoryServices;
use crate::capability::{HandleValue, InvocationReceiver};

pub(crate) trait DeviceServices: UserMemoryServices {
    fn device_firmware_read(
        &self,
        authority: &InvocationReceiver,
        node: u32,
        field: u32,
        name: &str,
    ) -> Result<alloc::vec::Vec<u8>, crate::device::assigned::service::MatchError>;
    fn device_claim_bundle(
        &self,
        authority: &InvocationReceiver,
        entries: &[(u32, u32, u64)],
        irq_node: u32,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn device_mmio(
        &self,
        device: &InvocationReceiver,
        offset: u64,
        width: u32,
        write: bool,
        value: u64,
    ) -> Result<u64, crate::vm::service::Error>;
    fn device_irq_pending(
        &self,
        device: &InvocationReceiver,
    ) -> Result<u64, crate::vm::service::Error>;
    fn device_irq_complete(
        &self,
        device: &InvocationReceiver,
        sequence: u64,
        asserted: bool,
    ) -> Result<(), crate::vm::service::Error>;

    fn device_dma_fault_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<[u8; 40], crate::vm::service::Error>;

    fn device_profile_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<[u8; 32], crate::vm::service::Error>;
    fn device_resource_info(
        &self,
        device: &InvocationReceiver,
        index: u32,
    ) -> Result<[u8; 32], crate::vm::service::Error>;
    fn claim_device_matching(
        &self,
        authority: &InvocationReceiver,
        profile: u32,
        identity_kind: u32,
        identity: &str,
    ) -> Result<HandleValue, crate::device::assigned::service::MatchError>;
    fn claim_device(
        &self,
        authority: &InvocationReceiver,
        index: u32,
    ) -> Result<HandleValue, crate::vm::service::Error>;
    fn physical_device_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<crate::device::assigned::Info, crate::vm::service::Error>;
    fn vmo_dma_extent(
        &self,
        authority: &InvocationReceiver,
        vmo: HandleValue,
        offset: u64,
        length: u64,
    ) -> Result<crate::device::assigned::DmaExtent, crate::vm::service::Error>;
    fn assign_physical_device(
        &self,
        pending: &InvocationReceiver,
        device: HandleValue,
        base: u64,
        irq: u32,
    ) -> Result<(), crate::vm::service::Error>;
}
