// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Whole PCI functions with host-owned configuration and interrupt routing.
//!
//! The host owns configuration, BAR placement and MSI routing. Function MMIO
//! is opaque: Linux owns its device drivers. Generic ECAM admits modern virtio
//! endpoints only behind an active host SMMU, using mediated `INTx` delivery.
//! The BCM2712 translation has no DMA isolation and requires a trusted I/O
//! owner. That implementation admits one direct, single-function type-0
//! endpoint behind one live BCM2712 x4 root port. It configures bus numbers
//! before enumeration, retains or relocates outbound windows into reserved
//! CPU MMIO, and installs validated DT inbound DMA ranges. Its exclusive owner
//! serializes the configuration selector; multiple functions will require a
//! shared host-bridge configuration lock.
//!
//! Disabling bus mastering is not a DMA-drain certificate. Modern virtio has a
//! standard reset acknowledgement; other active functions remain quarantined
//! until a separately proven reset protocol exists.

mod bcm2712;
mod ecam;
mod transport;
pub use transport::Transport;
mod firmware;
pub mod model;
pub use bcm2712::{
    DMA_OFFSET, DmaStage, DmaState, LinkFailure, LinkState, OutboundFailure, OutboundState,
    OutboundWriteState, inbound_size,
};
pub use firmware::owns_handoff_node;
pub use model::FunctionState;

use model::{BarAccess, MsixLayout};
use {
    crate::platform::DriverServices, crate::platform::MmioResource,
    crate::platform::PermanentMmioMapping, crate::platform::PlatformDevice,
};
use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};
use {hyper::mm::ranges::PhysicalRange, hyper_platform::fdt::NodeId};

pub const APERTURE_SIZE: u64 = 0x80_0000;
pub const ECAM_SIZE: u64 = 0x10_0000;
pub const MSI_FRAME_OFFSET: u64 = 0x10_0000;
pub const MAX_VECTORS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Firmware,
    Mapping,
    Link(LinkState),
    Identity,
    Handoff,
    BarType { index: u32, value: u32 },
    BarSize { index: u32, address: u64, mask: u64 },
    BarPlacement,
    BarOverlap { first: u32, second: u32 },
    Outbound(OutboundState),
    OutboundWrite(OutboundWriteState),
    Dma(DmaState),
    Interrupt,
}

#[derive(Clone, Copy)]
struct Registers(PermanentMmioMapping, fn());
impl Registers {
    fn read(self, offset: usize) -> u32 {
        self.read_width(offset, 4) as u32
    }
    fn write(self, offset: usize, value: u32) {
        self.write_width(offset, 4, u64::from(value));
    }
    fn read_width(self, offset: usize, width: usize) -> u64 {
        debug_assert!(
            offset.is_multiple_of(width)
                && offset as u64 + width as u64 <= self.0.resource().size()
        );
        (self.1)();
        let address = self.0.virtual_start() + offset;
        // SAFETY: construction retains a permanent device mapping. All callers
        // validate extent, alignment and width before dispatch. The assignment
        // lock serializes configuration, guest accesses and retirement.
        let result = unsafe {
            match width {
                1 => u64::from(core::ptr::with_exposed_provenance::<u8>(address).read_volatile()),
                2 => u64::from(core::ptr::with_exposed_provenance::<u16>(address).read_volatile()),
                4 => u64::from(core::ptr::with_exposed_provenance::<u32>(address).read_volatile()),
                8 => core::ptr::with_exposed_provenance::<u64>(address).read_volatile(),
                _ => hyper::debug::invariant_failure("PCI register width invariant"),
            }
        };
        (self.1)();
        result
    }
    fn write_width(self, offset: usize, width: usize, value: u64) {
        debug_assert!(
            offset.is_multiple_of(width)
                && offset as u64 + width as u64 <= self.0.resource().size()
        );
        (self.1)();
        let address = self.0.virtual_start() + offset;
        // SAFETY: same permanent mapping and serialized bounded access as read.
        unsafe {
            match width {
                1 => core::ptr::with_exposed_provenance_mut::<u8>(address)
                    .write_volatile(value as u8),
                2 => core::ptr::with_exposed_provenance_mut::<u16>(address)
                    .write_volatile(value as u16),
                4 => core::ptr::with_exposed_provenance_mut::<u32>(address)
                    .write_volatile(value as u32),
                8 => core::ptr::with_exposed_provenance_mut::<u64>(address).write_volatile(value),
                _ => hyper::debug::invariant_failure("PCI register width invariant"),
            }
        };
        (self.1)();
    }
}

#[derive(Clone, Copy)]
pub struct Bar {
    pub mapping: PermanentMmioMapping,
    pub offset: u64,
    pub flags: u32,
    pub index: u32,
}

struct BcmTransport {
    bridge: bcm2712::HostBridge,
    mip: Registers,
    bars: [Option<Bar>; 6],
    layouts: [Option<model::BarLayout>; 6],
    table: Registers,
    pending: Registers,
    msix: MsixLayout,
    msix_cap: usize,
    identity: u32,
    class_revision: u32,
    subsystem: u32,
    message_address: u64,
    order: fn(),
}

pub struct Prepared {
    pub firmware: NodeId,
    pub bars: [Option<Bar>; 6],
    pub interrupt: u32,
    pub interrupt_count: u32,
    pub transport: Transport,
}

impl BcmTransport {
    pub fn discover<B: Barrier>(
        nodes: &[PlatformDevice],
        services: &dyn DriverServices,
    ) -> Result<Option<Prepared>, Error> {
        let Some(topology) = firmware::Topology::discover(nodes)? else {
            return Ok(None);
        };
        let order = || B::data_memory(BarrierDomain::FullSystem, BarrierAccess::All);
        let map = |resource: MmioResource, size| {
            services
                .map_mmio(resource)
                .and_then(|mapping| mapping.validate_window(size, 4))
                .map(|mapping| Registers(mapping, order as fn()))
                .map_err(|_| Error::Mapping)
        };
        let bridge_resource = *topology.bridge.registers().first().ok_or(Error::Firmware)?;
        let bridge = bcm2712::HostBridge::prepare(map(bridge_resource, 0x9004)?)?;
        let mip_resource = *topology.mip.registers().first().ok_or(Error::Firmware)?;
        let mip = map(mip_resource, 0xc0)?;
        let (msix_cap, count, table_descriptor, pending_descriptor) = bridge.msix()?;
        bridge.write(
            msix_cap,
            (bridge.read(msix_cap) & !0x8000_0000) | 0x4000_0000,
        );
        let mut layouts = bridge.bars()?;
        if layouts.iter().all(Option::is_none) || !model::place_bars(&mut layouts) {
            return Err(Error::BarPlacement);
        }
        let msix = MsixLayout {
            count: count as u32,
            table: table_descriptor,
            pending: pending_descriptor,
        };
        if !msix.valid(&layouts) {
            return Err(Error::Interrupt);
        }
        bridge.prepare_outbound(&layouts, topology.outbound)?;
        let mut bars = [None; 6];
        for (index, layout) in layouts
            .iter()
            .enumerate()
            .filter_map(|(index, layout)| layout.map(|layout| (index, layout)))
        {
            let physical = bridge.bar_resource(layout, topology.outbound)?;
            if let Some(other) = bars.iter().flatten().find(|bar: &&Bar| {
                bar.mapping
                    .resource()
                    .physical_range()
                    .overlaps(physical.physical_range())
            }) {
                return Err(Error::BarOverlap {
                    first: other.index,
                    second: index as u32,
                });
            }
            bars[index] = Some(Bar {
                mapping: map(physical, layout.size)?.0,
                offset: layout.offset,
                flags: layout.flags,
                index: index as u32,
            });
        }
        let table_bar = bars[(table_descriptor & 7) as usize].ok_or(Error::Interrupt)?;
        let pending_bar = bars[(pending_descriptor & 7) as usize].ok_or(Error::Interrupt)?;
        let table = map(
            subrange(
                table_bar.mapping.resource(),
                u64::from(table_descriptor & !7),
                count as u64 * 16,
            )?,
            count as u64 * 16,
        )?;
        let pending_bytes = (count as u64).div_ceil(64) * 8;
        let pending = map(
            subrange(
                pending_bar.mapping.resource(),
                u64::from(pending_descriptor & !7),
                pending_bytes,
            )?,
            pending_bytes,
        )?;
        bridge.configure_dma(&topology.dma)?;
        // MIP register layout and inbound address encoding are host-controller
        // mechanisms. No endpoint function register is read or written here.
        for offset in [0x40, 0x50, 0x60, 0x70, 0x20, 0x30] {
            mip.write(offset, u32::MAX);
        }
        for vector in 0..count {
            table.write(vector * 16 + 12, 1);
            table.write(vector * 16, topology.message_address as u32);
            table.write(vector * 16 + 4, (topology.message_address >> 32) as u32);
            table.write(vector * 16 + 8, vector as u32);
        }
        let transport = Self {
            bridge,
            mip,
            bars,
            layouts,
            table,
            pending,
            msix,
            msix_cap,
            identity: bridge.read(0),
            class_revision: bridge.read(8),
            subsystem: bridge.read(0x2c),
            message_address: topology.message_address,
            order,
        };
        Ok(Some(Prepared {
            firmware: topology.function.id(),
            bars,
            interrupt: topology.host_irq,
            interrupt_count: count as u32,
            transport: Transport::bcm(transport),
        }))
    }
    pub const fn identity(&self) -> u32 {
        self.identity
    }
    pub const fn dma_offset(&self) -> u64 {
        DMA_OFFSET
    }
    pub const fn bars(&self) -> &[Option<Bar>; 6] {
        &self.bars
    }
    /// Admission validates that the complete aperture and SPI range fit the VM.
    pub fn state(&self, guest_base: u64, irq_base: u32) -> FunctionState {
        FunctionState::new(
            self.layouts,
            self.msix,
            self.identity,
            self.class_revision,
            self.subsystem,
            guest_base,
            irq_base,
        )
    }
    pub fn activate(&self, state: &mut FunctionState) {
        state.activate();
        let mask = if self.msix.count == 64 {
            u64::MAX
        } else {
            (1u64 << self.msix.count) - 1
        };
        self.mip.write(0x40, !(mask as u32));
        self.mip.write(0x50, !((mask >> 32) as u32));
        self.sync_control(state);
    }
    /// Gating is best effort, not a reset or proof of completed DMA. The caller
    /// must quarantine backing and ownership after any active assignment.
    pub fn stop(&self, state: &mut FunctionState) {
        state.stop();
        self.mip.write(0x40, u32::MAX);
        self.mip.write(0x50, u32::MAX);
        self.sync_control(state);
    }
    pub fn read(&self, state: &mut FunctionState, offset: usize, width: usize) -> Option<u64> {
        if (offset as u64) < ECAM_SIZE {
            return state.config_read(offset, width);
        }
        if (MSI_FRAME_OFFSET..MSI_FRAME_OFFSET + 4096).contains(&(offset as u64)) {
            return state.frame_read(offset - MSI_FRAME_OFFSET as usize, width);
        }
        match state.bar_access(offset, width)? {
            BarAccess::Msix { vector, offset } => state.table_read(vector, offset, width),
            BarAccess::Pending { offset } => Some(self.pending.read_width(offset, width)),
            BarAccess::Registers { index, offset } => {
                Some(Registers(self.bars[index]?.mapping, self.order).read_width(offset, width))
            }
        }
    }
    pub fn write(
        &self,
        state: &mut FunctionState,
        offset: usize,
        width: usize,
        value: u64,
    ) -> bool {
        if (offset as u64) < ECAM_SIZE {
            if !state.config_write(offset, width, value) {
                return false;
            }
            if offset & !3 == 4 || offset & !3 == model::MSIX_CAPABILITY {
                self.sync_control(state);
            }
            return true;
        }
        if (MSI_FRAME_OFFSET..MSI_FRAME_OFFSET + 4096).contains(&(offset as u64)) {
            return state.frame_write(offset - MSI_FRAME_OFFSET as usize, width, value);
        }
        match state.bar_access(offset, width) {
            Some(BarAccess::Msix { vector, offset }) => {
                if !state.table_write(vector, offset, width, value) {
                    return false;
                }
                self.sync_vector(state, vector);
                true
            }
            Some(BarAccess::Pending { .. }) => true, // PCI MSI-X PBA is read-only.
            Some(BarAccess::Registers { index, offset }) => {
                let Some(bar) = self.bars[index] else {
                    return false;
                };
                Registers(bar.mapping, self.order).write_width(offset, width, value);
                true
            }
            None => false,
        }
    }
    fn sync_vector(&self, state: &FunctionState, vector: usize) {
        // Mask before changing any part of the physical message. Guest table
        // writes may be partial; an invalid intermediate route remains masked.
        self.table.write(vector * 16 + 12, 1);
        self.table.write(vector * 16, self.message_address as u32);
        self.table
            .write(vector * 16 + 4, (self.message_address >> 32) as u32);
        self.table.write(vector * 16 + 8, vector as u32);
        self.table
            .write(vector * 16 + 12, u32::from(state.msi_irq(vector).is_none()));
        let _ = self.table.read(vector * 16 + 12);
    }
    fn sync_control(&self, state: &FunctionState) {
        let control = self.bridge.read(self.msix_cap);
        self.bridge
            .write(self.msix_cap, (control & !0x8000_0000) | 0x4000_0000);
        for vector in 0..self.msix.count as usize {
            self.sync_vector(state, vector);
        }
        self.bridge.write(
            self.msix_cap,
            (control & !0xc000_0000)
                | (u32::from(state.msix_enabled()) << 31)
                | (u32::from(state.msix_masked()) << 30),
        );
        self.bridge.bus_master(state.bus_master());
    }
}

fn subrange(resource: MmioResource, offset: u64, size: u64) -> Result<MmioResource, Error> {
    offset
        .checked_add(size)
        .filter(|end| *end <= resource.size())
        .ok_or(Error::Mapping)?;
    let range = PhysicalRange::new(
        resource.start().checked_add(offset).ok_or(Error::Mapping)?,
        size,
    )
    .ok_or(Error::Mapping)?;
    // SAFETY: this is a checked subrange of a firmware-owned MMIO capability.
    Ok(unsafe { MmioResource::from_physical_range(range) })
}
