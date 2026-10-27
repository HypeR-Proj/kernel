// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::super::{Profile, Window, service};
use super::{Error, PhysicalDevice};
use hyper_drivers::pci;

impl PhysicalDevice {
    pub(crate) fn dma_stream(&self) -> Option<u32> {
        match &self.claim.hardware.profile {
            Profile::Pci(transport) => transport.stream(),
            _ => None,
        }
    }
    pub(crate) fn info(&self) -> service::Info {
        service::Info {
            device_id: match self.claim.hardware.profile {
                Profile::Virtio(kind) => kind.device_id(),
                _ => 0,
            },
            transport_version: if matches!(self.claim.hardware.profile, Profile::Virtio(_)) {
                2
            } else {
                0
            },
            mmio_size: self.claim.hardware.mapping.resource().size(),
        }
    }

    pub(crate) fn profile_info(&self) -> [u8; 32] {
        let hw = &self.claim.hardware;
        let (count, identity, dma) = match &hw.profile {
            Profile::Pci(transport) => (
                2 + transport.bars().iter().flatten().count() as u32,
                transport.identity(),
                transport.dma_offset(),
            ),
            _ => (1 + hw.extra.iter().flatten().count() as u32, 0, 0),
        };
        let mut output = [0; 32];
        output[0..4].copy_from_slice(&hw.profile.id().to_le_bytes());
        output[4..8].copy_from_slice(&hw.interrupt_count.to_le_bytes());
        output[8..12].copy_from_slice(&count.to_le_bytes());
        output[12..16].copy_from_slice(&identity.to_le_bytes());
        output[16..24].copy_from_slice(&dma.to_le_bytes());
        output[24..32].copy_from_slice(&hw.profile.aperture().to_le_bytes());
        output
    }

    pub(crate) fn resource_info(&self, index: u32) -> Result<[u8; 32], Error> {
        if let Profile::Pci(transport) = &self.claim.hardware.profile {
            let (kind, flags, offset, length, bus) = match index {
                0 => (0x100, 0, 0, pci::ECAM_SIZE, 0),
                1 if transport.uses_intx() => (0x102, 0, 0, 0, 0),
                1 => (0x101, 0, pci::MSI_FRAME_OFFSET, 4096, 0),
                _ => {
                    let bar = transport
                        .bars()
                        .iter()
                        .flatten()
                        .nth(index as usize - 2)
                        .ok_or(Error::InvalidArgument)?;
                    (
                        0x200 + bar.index,
                        bar.flags,
                        bar.offset,
                        bar.mapping.resource().size(),
                        bar.offset,
                    )
                }
            };
            return Ok(resource(kind, flags, offset, length, bus));
        }
        let window = if index == 0 {
            Window {
                mapping: self.claim.hardware.mapping,
                offset: 0,
            }
        } else {
            self.claim
                .hardware
                .extra
                .get(index as usize - 1)
                .copied()
                .flatten()
                .ok_or(Error::InvalidArgument)?
        };
        Ok(resource(
            index + 1,
            0,
            window.offset as u64,
            window.mapping.resource().size(),
            0,
        ))
    }
}

fn resource(kind: u32, flags: u32, offset: u64, length: u64, bus: u64) -> [u8; 32] {
    let mut output = [0; 32];
    output[0..4].copy_from_slice(&kind.to_le_bytes());
    output[4..8].copy_from_slice(&flags.to_le_bytes());
    output[8..16].copy_from_slice(&offset.to_le_bytes());
    output[16..24].copy_from_slice(&length.to_le_bytes());
    output[24..32].copy_from_slice(&bus.to_le_bytes());
    output
}

impl PhysicalDevice {
    pub(super) fn diagnostic(
        &self,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::Details, crate::object::diagnostics::DetailError> {
        use {
            crate::object::diagnostics::DetailError, crate::object::diagnostics::DetailRecord,
            crate::object::diagnostics::Details,
        };
        let hw = &self.claim.hardware;
        let resources = match &hw.profile {
            Profile::Pci(transport) => transport.bars().iter().flatten().count(),
            _ => 1 + hw.extra.iter().flatten().count(),
        };
        if cursor == 0 {
            let state = self.state.with(|state| match state {
                super::State::Claimed => 1,
                super::State::Attached => 2,
                super::State::Active(_) => 3,
                super::State::Retired => 4,
                super::State::Quarantined => 5,
            });
            return Ok(Details {
                record: DetailRecord::Device {
                    profile: u64::from(hw.profile.id()),
                    device_id: u64::from(self.info().device_id),
                    pci_identity: match &hw.profile {
                        Profile::Pci(t) => u64::from(t.identity()),
                        _ => 0,
                    },
                    irq_domain: u64::from(hw.domain.diagnostic_id()),
                    interrupt: u64::from(hw.interrupt.get()),
                    interrupt_count: u64::from(hw.interrupt_count),
                    state,
                    resource_count: resources as u64,
                },
                next_cursor: if resources == 0 { 0 } else { 1 },
            });
        }
        let index = usize::try_from(cursor - 1).map_err(|_| DetailError::InvalidCursor)?;
        if index >= resources {
            return Err(DetailError::InvalidCursor);
        }
        let record = if let Profile::Pci(transport) = &hw.profile {
            let bar = transport
                .bars()
                .iter()
                .flatten()
                .nth(index)
                .ok_or(DetailError::InvalidCursor)?;
            DetailRecord::DeviceResource {
                kind: 0x200 + u64::from(bar.index),
                base: bar.mapping.resource().start(),
                length: bar.mapping.resource().size(),
                offset: bar.offset,
                flags: u64::from(bar.flags),
            }
        } else {
            let window = if index == 0 {
                Window {
                    mapping: hw.mapping,
                    offset: 0,
                }
            } else {
                hw.extra
                    .iter()
                    .flatten()
                    .nth(index - 1)
                    .copied()
                    .ok_or(DetailError::InvalidCursor)?
            };
            DetailRecord::DeviceResource {
                kind: cursor,
                base: window.mapping.resource().start(),
                length: window.mapping.resource().size(),
                offset: window.offset as u64,
                flags: 0,
            }
        };
        Ok(Details {
            record,
            next_cursor: if index + 1 < resources { cursor + 1 } else { 0 },
        })
    }
}
