// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native device service calls under borrowed Process authority.

use super::ProcessServices;
use {crate::capability::HandleValue, crate::capability::InvocationReceiver};

impl crate::abi::native::DeviceServices for ProcessServices<'_> {
    fn device_firmware_read(
        &self,
        authority: &InvocationReceiver,
        node: u32,
        field: u32,
        name: &str,
    ) -> Result<alloc::vec::Vec<u8>, crate::device::assigned::service::MatchError> {
        crate::device::assigned::service::firmware_read(authority, node, field, name)
    }
    fn device_claim_bundle(
        &self,
        authority: &InvocationReceiver,
        entries: &[(u32, u32, u64)],
        irq_node: u32,
    ) -> Result<HandleValue, crate::vm::service::Error> {
        crate::device::assigned::service::claim_bundle(self.process, authority, entries, irq_node)
    }
    fn device_mmio(
        &self,
        device: &InvocationReceiver,
        offset: u64,
        width: u32,
        write: bool,
        value: u64,
    ) -> Result<u64, crate::vm::service::Error> {
        crate::device::assigned::service::mmio(device, offset, width, write, value)
    }
    fn device_irq_pending(
        &self,
        device: &InvocationReceiver,
    ) -> Result<u64, crate::vm::service::Error> {
        crate::device::assigned::service::irq_pending(device)
    }
    fn device_irq_complete(
        &self,
        device: &InvocationReceiver,
        sequence: u64,
        asserted: bool,
    ) -> Result<(), crate::vm::service::Error> {
        crate::device::assigned::service::irq_complete(device, sequence, asserted)
    }

    fn device_dma_fault_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<[u8; 40], crate::vm::service::Error> {
        crate::device::assigned::service::dma_fault_info(device)
    }

    fn device_profile_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<[u8; 32], crate::vm::service::Error> {
        crate::device::assigned::service::profile_info(device)
    }
    fn device_resource_info(
        &self,
        device: &InvocationReceiver,
        index: u32,
    ) -> Result<[u8; 32], crate::vm::service::Error> {
        crate::device::assigned::service::resource_info(device, index)
    }
    fn claim_device_matching(
        &self,
        authority: &InvocationReceiver,
        profile: u32,
        identity_kind: u32,
        identity: &str,
    ) -> Result<HandleValue, crate::device::assigned::service::MatchError> {
        crate::device::assigned::service::claim_matching(
            self.process,
            authority,
            profile,
            identity_kind,
            identity,
        )
    }
    fn claim_device(
        &self,
        authority: &InvocationReceiver,
        index: u32,
    ) -> Result<HandleValue, crate::vm::service::Error> {
        crate::device::assigned::service::claim(self.process, authority, index)
    }
    fn physical_device_info(
        &self,
        device: &InvocationReceiver,
    ) -> Result<crate::device::assigned::Info, crate::vm::service::Error> {
        crate::device::assigned::service::info(device)
    }
    fn vmo_dma_extent(
        &self,
        authority: &InvocationReceiver,
        vmo: HandleValue,
        offset: u64,
        length: u64,
    ) -> Result<crate::device::assigned::DmaExtent, crate::vm::service::Error> {
        crate::device::assigned::service::dma_extent(self.process, authority, vmo, offset, length)
    }
    fn assign_physical_device(
        &self,
        pending: &InvocationReceiver,
        device: HandleValue,
        base: u64,
        irq: u32,
    ) -> Result<(), crate::vm::service::Error> {
        crate::device::assigned::service::assign(self.process, pending, device, base, irq)
    }
}
